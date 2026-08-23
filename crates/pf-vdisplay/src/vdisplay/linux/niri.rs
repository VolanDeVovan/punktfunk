//! niri virtual-output backend via niri IPC + niri's own Mutter ScreenCast D-Bus interface:
//!
//! 1. `niri msg -j create-virtual-output --name <n> --width W --height H --refresh-rate Hz`
//!    adds a headless output at the client's exact mode and answers `{"name": "..."}`. The mode is
//!    fixed at creation (there is no `mode --custom` follow-up like sway's), and it is also the
//!    output's refresh clock: `render_virtual_output` submits presentation feedback and queues an
//!    estimated-vblank timer, which is what paces the screencast.
//! 2. `org.gnome.Mutter.ScreenCast.CreateSession({})` → `Session.RecordMonitor(<name>)` →
//!    `Session.Start()` → the node id arrives on `PipeWireStreamAdded`.
//! 3. Teardown is RAII: drop ends the D-Bus connection (which is the cast's lifetime) and then runs
//!    `niri msg remove-virtual-output <name>`.
//!
//! ── Why this is neither of its two siblings ────────────────────────────────
//! Unlike **wlroots/sway**, no portal is involved. niri implements the Mutter ScreenCast interface
//! itself, so `RecordMonitor` names the output directly — none of xdpw's headless-chooser
//! machinery (managed config file, portal restart, `SELECTION_LOCK`, sandboxed remote fd) is
//! needed, and the node lands on the user's own PipeWire daemon.
//!
//! Unlike **Mutter**, the cast is NOT anchored on `org.gnome.Mutter.RemoteDesktop`: niri does not
//! implement that interface (niri-wm/niri#390 is open). A bare `ScreenCast.CreateSession({})` is
//! accepted and drives the stream on its own — verified on-glass against niri 26.04, which answered
//! `PipeWireStreamAdded` for a `RecordMonitor` on a physical head with no portal dialog and no
//! anchor session.
//!
//! ── Requirements ───────────────────────────────────────────────────────────
//! * niri built with virtual-output support (niri-wm/niri#3800, unmerged upstream). Without it
//!   `create` fails at the IPC call and [`create`](VirtualDisplay::create) says so in as many
//!   words; [`stream_existing_output`] (the mirror path) works on stock niri.
//! * niri built with the `xdp-gnome-screencast` feature (nixpkgs' default) — that is what
//!   registers `org.gnome.Mutter.ScreenCast` and what renders each output for the cast. Note the
//!   render call sits OUTSIDE niri's `is_virtual` branch, so virtual outputs are cast like any
//!   other head.
//! * The host running inside the niri session's environment (`NIRI_SOCKET` for `niri msg`, the
//!   session bus for D-Bus). `niri --session` imports both into the systemd user manager, so a
//!   `systemd --user` host inherits them.
//! * The PATCHED `niri` first on the unit's `PATH` — this shells out to `niri msg`, so a stock
//!   binary shadowing it turns `create` into "unrecognized subcommand".
//!
//! ── A caveat worth knowing before debugging a stall ────────────────────────
//! Frames follow DAMAGE, not the clock. A virtual output with nothing changing on it renders once
//! and then stops, exactly as a physical head would — measured: an EMPTY virtual output delivers
//! one buffer and the capturer then times out at 10 s ("format negotiated but no buffers arrived"),
//! while the same output with one animating window streamed 360 frames in 3.09 s against a 120 Hz
//! mode. A real desktop damages continuously enough (cursor, bar, clock) that this does not arise,
//! but a deliberately frozen screen will look like a capture failure rather than a still image.
//! Upstream is aware — the PR discussion floats a synthetic refresh for idle rendering.

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use ashpd::zbus;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use zbus::zvariant::{OwnedObjectPath, Value};

const BUS_SC: &str = "org.gnome.Mutter.ScreenCast";

/// Mutter/niri cursor modes (niri's `CursorMode`: `Hidden = 0, Embedded = 1, Metadata = 2`).
/// `Embedded` has the compositor paint the pointer into the frames; `Metadata` ships it as
/// `SPA_META_Cursor` for the host to composite or forward over the cursor channel.
const CURSOR_EMBEDDED: u32 = 1;
const CURSOR_METADATA: u32 = 2;

/// Time budget for a `niri msg` call. Every compositor query shells out, and an unbounded one can
/// wedge the session thread forever (see [`crate::proc`]).
const MSG_BUDGET: Duration = Duration::from_secs(5);

/// Names our virtual outputs `pf-1`, `pf-2`, … — process-wide so two concurrent sessions cannot
/// collide on a name. niri rejects a create whose name is already taken, so a collision would be a
/// hard failure rather than a silent share; this makes it not arise.
static OUTPUT_SEQ: AtomicU64 = AtomicU64::new(0);

/// The niri virtual-display driver. Stateless — each [`create`](VirtualDisplay::create) adds one
/// virtual output and spins up a D-Bus thread owning the cast on it.
pub struct NiriDisplay {
    /// Out-of-band cursor request (`set_hw_cursor`, the negotiated cursor channel): `Metadata` when
    /// on, `Embedded` (the compositor paints it) otherwise. Same contract as the other backends.
    hw_cursor: bool,
}

impl NiriDisplay {
    pub fn new() -> Result<Self> {
        Ok(NiriDisplay { hw_cursor: false })
    }
}

/// niri is usable when the host runs inside a niri session — signalled by `NIRI_SOCKET` (the IPC
/// socket `niri msg` needs). Cheap env check for the enumeration path, mirroring sway's `SWAYSOCK`.
pub fn is_available() -> bool {
    std::env::var_os("NIRI_SOCKET").is_some()
}

impl VirtualDisplay for NiriDisplay {
    fn name(&self) -> &'static str {
        "niri"
    }

    fn set_hw_cursor(&mut self, on: bool) {
        self.hw_cursor = on;
    }

    fn hw_cursor(&self) -> bool {
        self.hw_cursor
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        let requested = format!("pf-{}", OUTPUT_SEQ.fetch_add(1, Ordering::Relaxed) + 1);

        // niri fixes the mode at creation — there is no follow-up `mode --custom` to get wrong, and
        // the refresh rate doubles as the output's frame clock. `--refresh-rate` is whole Hz.
        let created = create_virtual_output(&requested, mode)?;
        // Own it from here on, so any error below unwinds into a remove instead of leaking an
        // output into the user's layout.
        let output = OutputGuard(created);
        let name = output.0.clone();

        let (node_id, stop) = record(&name, self.hw_cursor)?;
        tracing::info!(
            node_id,
            output = %name,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            "niri virtual output ready"
        );

        let mut out = VirtualOutput::owned(
            node_id,
            Some((mode.width, mode.height, mode.refresh_hz)),
            Box::new(Keepalive {
                _stop: stop,
                _output: output,
            }),
        );
        // The node lives on the user's own PipeWire daemon (like KWin/Mutter, unlike the
        // portal-based backends), so there is no sandboxed remote for the capturer to connect
        // through — and nothing that would keep the registry from pooling this display.
        out.remote_fd = None;
        out.ownership = DisplayOwnership::Owned;
        Ok(out)
    }
}

/// Drop order matters: stop the D-Bus thread first (its connection drop ends the cast), then remove
/// the output (fields drop in declaration order).
struct Keepalive {
    _stop: StopGuard,
    _output: OutputGuard,
}

/// Dropping this ends the D-Bus keepalive thread, closing its zbus connection — niri then tears the
/// screencast session down.
struct StopGuard(Arc<AtomicBool>);

impl Drop for StopGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Owns the created virtual output; dropping it removes it from niri.
struct OutputGuard(String);

impl Drop for OutputGuard {
    fn drop(&mut self) {
        // Deliberately NOT preceded by an `output <name> off`: removing a virtual output that is
        // disabled panics the compositor upstream ("trying to remove non-existing output"), so the
        // output stays enabled right up to the remove.
        match niri_msg(&["remove-virtual-output", &self.0]) {
            Ok(_) => tracing::info!(output = %self.0, "niri virtual output removed"),
            Err(e) => {
                tracing::warn!(output = %self.0, error = %format!("{e:#}"), "remove failed")
            }
        }
    }
}

/// `niri msg -j create-virtual-output …` → the name niri actually assigned.
///
/// The name is read back rather than assumed: niri picks `HEADLESS-N` when `--name` is omitted, and
/// reading the answer keeps this correct if it ever declines a requested name.
fn create_virtual_output(requested: &str, mode: Mode) -> Result<String> {
    let w = mode.width.clamp(1, u16::MAX as u32).to_string();
    let h = mode.height.clamp(1, u16::MAX as u32).to_string();
    let hz = mode.refresh_hz.max(1).to_string();
    let out = niri_msg_json(&[
        "create-virtual-output",
        "--name",
        requested,
        "--width",
        &w,
        "--height",
        &h,
        "--refresh-rate",
        &hz,
    ])
    .context(
        "niri msg create-virtual-output (does this niri have virtual-output support? \
         niri-wm/niri#3800 is unmerged upstream — a stock build has no such subcommand)",
    )?;
    out.get("name")
        .and_then(|n| n.as_str())
        .map(str::to_owned)
        .context("create-virtual-output: no `name` in the reply")
}

/// Record an **existing** output by connector — the monitor-mirror path
/// (`design/per-monitor-portal-capture.md` L2), and the one path here that works on a STOCK niri:
/// it creates nothing, so it needs no virtual-output support.
///
/// Same ScreenCast handshake as [`create`](VirtualDisplay::create), one call different in intent:
/// the output is someone else's, so nothing is torn down but the cast.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let (node_id, stop) = record(connector, hw_cursor)?;
    Ok(crate::mirror::MirrorStream {
        node_id,
        // niri publishes the node on the user's own PipeWire daemon — nothing to carry.
        remote_fd: None,
        // Not an xdg-portal session: `cursor-mode` was set directly on `RecordMonitor` and niri
        // honours it, so the request IS the answer and there is nothing to report back.
        cursor_mode: None,
        keepalive: Box::new(stop),
    })
}

/// Start a cast on `output` and return its PipeWire node id plus the flag that stops it.
///
/// The D-Bus connection IS the session's lifetime, so it lives on its own thread that parks until
/// the flag is set. Built before the wait so a timeout/failure arm still signals the thread rather
/// than leaving a permanent cast behind.
fn record(output: &str, hw_cursor: bool) -> Result<(u32, StopGuard)> {
    let (setup_tx, setup_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let output_thread = output.to_string();
    thread::Builder::new()
        .name("punktfunk-niri-cast".into())
        .spawn(move || cast_thread(setup_tx, stop_thread, output_thread, hw_cursor))
        .context("spawn niri screencast thread")?;
    // Built BEFORE the wait: each `bail!` below drops it, which signals the thread rather than
    // leaving a permanent cast on an output nobody is going to tear down.
    let guard = StopGuard(stop);
    let node_id = match setup_rx.recv_timeout(Duration::from_secs(20)) {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => bail!("niri screencast failed: {e}"),
        Err(_) => bail!("timed out recording the niri output {output:?}"),
    };
    Ok((node_id, guard))
}

/// Owns the D-Bus connection behind a cast: connect, hand back the node id, park until stopped,
/// then `Stop` the session.
fn cast_thread(
    setup_tx: Sender<Result<u32, String>>,
    stop: Arc<AtomicBool>,
    output: String,
    hw_cursor: bool,
) {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = setup_tx.send(Err(format!("build tokio runtime: {e}")));
            return;
        }
    };
    rt.block_on(async move {
        let session = match connect(&output, hw_cursor).await {
            Ok(s) => s,
            Err(e) => {
                let _ = setup_tx.send(Err(format!("{e:#}")));
                return;
            }
        };
        if setup_tx.send(Ok(session.node_id)).is_err() {
            // The opener already gave up (its `recv_timeout` fired), so nothing will ever drop this
            // session's keepalive. Unwind here rather than parking forever on the connection that
            // IS the cast's lifetime.
            tracing::warn!(
                node_id = session.node_id,
                output = %output,
                "niri: the opener gave up before the handshake finished — stopping the session \
                 instead of parking on it"
            );
            let _ = session.sc_session.call_method("Stop", &()).await;
            return;
        }
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let _ = session.sc_session.call_method("Stop", &()).await;
    });
}

/// The handshake: session bus → `ScreenCast.CreateSession({})` → `RecordMonitor(output)` →
/// subscribe → `Start()` → node id.
///
/// The subscription is taken **before** `Start` on purpose: `PipeWireStreamAdded` can otherwise
/// land while we are still subscribing.
async fn connect(output: &str, hw_cursor: bool) -> Result<NiriSession> {
    let conn = zbus::Connection::session()
        .await
        .context("connect session D-Bus")?;

    let sc = zbus::Proxy::new(&conn, BUS_SC, "/org/gnome/Mutter/ScreenCast", BUS_SC)
        .await
        .context(
            "ScreenCast proxy (is niri running, and built with the xdp-gnome-screencast feature?)",
        )?;
    // No `remote-desktop-session-id`: niri has no RemoteDesktop interface to anchor on, and its
    // ScreenCast session is self-standing.
    let props: HashMap<&str, Value> = HashMap::new();
    let sc_path: OwnedObjectPath = sc
        .call("CreateSession", &(props,))
        .await
        .context("ScreenCast.CreateSession")?;
    let sc_session = zbus::Proxy::new(
        &conn,
        BUS_SC,
        sc_path,
        "org.gnome.Mutter.ScreenCast.Session",
    )
    .await?;

    // `RecordMonitor(connector, properties) -> stream_path`. Only `cursor-mode` is set: the mode
    // was fixed when the output was created (or belongs to its owner, on the mirror path).
    let mut rec: HashMap<&str, Value> = HashMap::new();
    rec.insert(
        "cursor-mode",
        Value::from(if hw_cursor {
            CURSOR_METADATA
        } else {
            CURSOR_EMBEDDED
        }),
    );
    let stream_path: OwnedObjectPath = sc_session
        .call("RecordMonitor", &(output, rec))
        .await
        .with_context(|| format!("Session.RecordMonitor({output:?})"))?;

    let stream = zbus::Proxy::new(
        &conn,
        BUS_SC,
        stream_path,
        "org.gnome.Mutter.ScreenCast.Stream",
    )
    .await?;
    let mut added = stream
        .receive_signal("PipeWireStreamAdded")
        .await
        .context("subscribe PipeWireStreamAdded")?;
    sc_session
        .call_method("Start", &())
        .await
        .context("ScreenCast.Session.Start")?;
    let msg = tokio::time::timeout(Duration::from_secs(10), added.next())
        .await
        .map_err(|_| anyhow!("PipeWireStreamAdded did not arrive within 10s"))?
        .ok_or_else(|| anyhow!("signal stream ended before PipeWireStreamAdded"))?;
    let (node_id,): (u32,) = msg
        .body()
        .deserialize()
        .context("PipeWireStreamAdded body")?;

    Ok(NiriSession {
        sc_session,
        _conn: conn,
        node_id,
    })
}

/// The live session objects (held for the stream's lifetime) + the PipeWire node id.
struct NiriSession {
    sc_session: zbus::Proxy<'static>,
    /// The connection IS the session's lifetime — niri drops the cast when it closes.
    _conn: zbus::Connection,
    node_id: u32,
}

/// Could `name` be a virtual output a punktfunk host created? Ours are the only ones named `pf-*`
/// — niri's own generated name is `HEADLESS-N` and an operator's `create-virtual` output carries
/// whatever they called it, so unlike sway's blanket `HEADLESS-` prefix this really does attribute.
pub(crate) fn is_managed_output(name: &str) -> bool {
    name.starts_with("pf-")
}

/// Focus the streamed output so windows this session opens land ON it.
///
/// niri is an EXTEND topology: the virtual output sits beside the operator's heads, and a new
/// window goes to whatever monitor holds focus. Nothing else in the session moves it (the client's
/// pointer is confined to the streamed output), so without this every app the host launches opens
/// where the client cannot see it. The sway and Hyprland twins are `wlroots::focus_output` and
/// `hyprland::focus_output`; none of the three touches the operator's heads.
///
/// Best-effort: a failure costs window placement, not the session.
pub(crate) fn focus_output(name: &str) {
    match niri_msg(&focus_argv(name)) {
        Ok(_) => tracing::info!(output = %name, "focused the streamed virtual output"),
        Err(e) => tracing::warn!(
            output = %name, error = %format!("{e:#}"),
            "could not focus the streamed virtual output — apps this session launches may open on \
             a physical monitor instead of on the stream"
        ),
    }
}

/// The `niri msg` argv that focuses `name`, split out so a test pins its SHAPE.
///
/// niri's is an ACTION, not an output command: `msg action focus-monitor <name>`. The `output`
/// subcommand this file otherwise uses (`output <name> …`) has no focus verb at all, so reaching
/// for the familiar shape yields an unrecognized-subcommand error rather than a wrong focus.
fn focus_argv(name: &str) -> [&str; 3] {
    ["action", "focus-monitor", name]
}

/// Every output niri reports, for the streamed-screen pin and the console picker.
///
/// Unlike sway's headless outputs, ours are self-identifying: niri stamps every virtual output
/// `make = "niri"`, `model = "virtual"`, and ours additionally carry the `pf-` name we chose. Both
/// halves are needed for [`crate::monitors::PhysicalMonitor::managed`] to mean "ours": the stamp
/// alone would also claim a virtual output the USER declared with `create-virtual` in their niri
/// config, which is a head we neither made nor may tear down.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>> {
    let outputs = niri_msg_json(&["outputs"]).context("niri msg outputs")?;
    let map = outputs
        .as_object()
        .context("niri msg outputs: not a JSON object")?;
    let mut out: Vec<_> = map
        .iter()
        .map(|(connector, o)| {
            let make = o.get("make").and_then(|v| v.as_str()).unwrap_or("");
            let model = o.get("model").and_then(|v| v.as_str()).unwrap_or("");
            // `current_mode` indexes into `modes`; both are absent on a disabled output.
            let mode = o
                .get("current_mode")
                .and_then(|i| i.as_u64())
                .and_then(|i| o.get("modes")?.get(i as usize));
            let logical = o.get("logical").filter(|l| !l.is_null());
            let num = |v: Option<&serde_json::Value>, k: &str| {
                v.and_then(|v| v.get(k)).and_then(|v| v.as_f64())
            };
            crate::monitors::PhysicalMonitor {
                connector: connector.clone(),
                description: crate::monitors::describe(make, model, connector),
                width: num(mode, "width").unwrap_or(0.0) as u32,
                height: num(mode, "height").unwrap_or(0.0) as u32,
                // niri already reports refresh in mHz.
                refresh_mhz: num(mode, "refresh_rate").unwrap_or(0.0) as u32,
                x: num(logical, "x").unwrap_or(0.0) as i32,
                y: num(logical, "y").unwrap_or(0.0) as i32,
                scale: num(logical, "scale").filter(|s| *s > 0.0).unwrap_or(1.0),
                // niri has no primary output — the concept does not exist in its layout.
                primary: false,
                // A disabled output keeps its entry but loses its logical rectangle.
                enabled: logical.is_some(),
                managed: make == "niri" && model == "virtual" && connector.starts_with("pf-"),
            }
        })
        .collect();
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// Run `niri msg <args>`, returning stdout. `niri msg` exits non-zero with the reason on stderr, so
/// checking the status covers both a failed request and an unknown subcommand (which is how a niri
/// without virtual-output support answers `create-virtual-output`).
fn niri_msg(args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("niri");
    cmd.arg("msg").args(args);
    let out = crate::proc::output_within(&mut cmd, MSG_BUDGET)
        .context("run niri msg (is niri installed and NIRI_SOCKET set?)")?;
    if !out.status.success() {
        bail!(
            "niri msg {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// [`niri_msg`] with `-j`, parsed. The flag goes BEFORE the subcommand — `niri msg` puts its own
/// options ahead of the command, so a trailing `-j` is read as an argument to the subcommand.
fn niri_msg_json(args: &[&str]) -> Result<serde_json::Value> {
    let mut argv = vec!["-j"];
    argv.extend_from_slice(args);
    let raw = niri_msg(&argv)?;
    serde_json::from_str(&raw).with_context(|| format!("parse niri msg {args:?} output"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// niri spells focus as an ACTION — `msg action focus-monitor <name>` — while every other call
    /// in this file is either a top-level subcommand (`create-virtual-output`) or the `output
    /// <name> <verb>` shape. Reaching for the familiar `output` noun yields an unrecognized
    /// subcommand, and the only symptom would be the bug the call exists to fix: apps opening on
    /// the operator's monitor while the stream shows a bare desktop.
    #[test]
    fn focus_goes_through_the_action_subcommand() {
        assert_eq!(focus_argv("pf-2"), ["action", "focus-monitor", "pf-2"]);
    }

    /// `pf-` really attributes, unlike sway's blanket `HEADLESS-`: niri's own generated name is
    /// `HEADLESS-N` and an operator's `create-virtual` output carries whatever they called it, so
    /// neither may be mistaken for ours. This is what keeps `focus_output` off a head we did not
    /// make and `check_mirrorable` honest about refusing to mirror our own display.
    #[test]
    fn only_our_own_naming_scheme_counts_as_managed() {
        assert!(is_managed_output("pf-1"));
        assert!(is_managed_output("pf-12"));
        // niri's own default virtual output — virtual, but not ours.
        assert!(!is_managed_output("HEADLESS-1"));
        // An operator's `create-virtual` output, and a physical head.
        assert!(!is_managed_output("ipad"));
        assert!(!is_managed_output("DP-2"));
    }
}
