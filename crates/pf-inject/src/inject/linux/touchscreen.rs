//! Virtual multitouch screen ("Punktfunk Touchscreen"): a uinput device carrying the wire's touch
//! plane as REAL contacts (kernel MT protocol B), for the injector backends that have no
//! virtual-touch protocol to reach for — today the wlroots one, which is the whole niri/sway
//! desktop family.
//!
//! Deliberately a **uinput device, not a compositor-protocol citizen**, for the same reason
//! [`super::pen`] is: no virtual-TOUCH protocol exists in wlr-protocols at all (the family ships
//! `zwlr_virtual_pointer_v1` and `zwp_virtual_keyboard_v1` and stops there), so a client that
//! picked touch passthrough had its fingers dropped on the floor by the wlroots backend — the
//! arm there was literally an empty `{}`. libinput consumes evdev
//! touchscreens everywhere, and udev's `input_id` builtin classifies this one from its
//! capabilities (`ABS_X`/`ABS_Y` + `BTN_TOUCH` + `INPUT_PROP_DIRECT` ⇒ `ID_INPUT_TOUCHSCREEN`),
//! so the compositor sees a screen someone is touching, with no protocol to negotiate.
//!
//! **Output mapping is the compositor's, and it needs telling.** A touchscreen is mapped onto one
//! output, and the compositors of this family default to *some* output rather than the streamed
//! one — niri's `output_for_touch` falls back to the first output in its global space, i.e. the
//! operator's physical head. Unlike the absolute POINTER path there is nothing to bind here (the
//! wlr protocol's per-output anchor has no touch counterpart), so the pin lives in the
//! compositor's own config, keyed on the streamed output's name:
//!
//! ```text
//! # niri
//! input { touch { map-to-output "pf-1" } }
//! # sway
//! input "1209:5054:Punktfunk_Touchscreen" map_to_output pf-1
//! ```
//!
//! which is why the niri vdisplay backend hands out the LOWEST FREE `pf-N` rather than an
//! ever-growing counter: a name that changes every session is a name no config can pin. Sway can
//! key the rule on this device by identity, niri's touch config is global — either way the target
//! is the output name, so both want it stable.
//!
//! Contacts are keyed by the wire's finger id and mapped onto compact MT slots; the tracking id is
//! a monotonic counter, never the wire id, because the wire reuses ids as soon as a finger lifts
//! and MT-B identifies a NEW touch by a new tracking id in the slot.
//!
//! ioctl numbers/struct layouts mirror [`super::pen`] (same kernel generation, same verification);
//! each backend file stays self-contained by convention.

use anyhow::{bail, Result};
use punktfunk_core::input::{InputEvent, InputKind};
use std::os::fd::{AsRawFd, OwnedFd};

// ioctls (x86_64).
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556e;

// input-event-codes.h subset.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const SYN_REPORT: u16 = 0;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const BTN_TOUCH: u16 = 0x14a;
/// The contact lands where the finger is, on the display this device is mapped to — libinput then
/// maps the full ABS range onto that output rect, which is exactly the wire's normalized
/// coordinate contract. Without it libinput would take the device for an indirect one (a touchpad)
/// and turn absolute contacts into pointer motion.
const INPUT_PROP_DIRECT: libc::c_int = 0x01;

/// Simultaneous contacts the device declares (MT slots). Ten fingers is both the platform maximum
/// the Windows synthetic-pointer path uses and more than any client sends.
const MAX_CONTACTS: usize = 10;

/// Full-scale position on both axes: the wire's coordinates are rescaled onto this range, and
/// libinput maps it onto the mapped output's rect.
const ABS_MAX: i32 = 65535;

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct AbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    absinfo: AbsInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEventRaw {
    time: libc::timeval,
    type_: u16,
    code: u16,
    value: i32,
}

fn ioctl_int(fd: i32, req: libc::c_ulong, arg: libc::c_int, what: &str) -> Result<()> {
    // SAFETY: every caller passes a UI_SET_*/UI_DEV_* request whose argument the kernel reads as a
    // plain int; `fd` is a live uinput fd owned by the caller. No memory is handed over.
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

fn ioctl_ptr<T>(fd: i32, req: libc::c_ulong, arg: *mut T, what: &str) -> Result<()> {
    // SAFETY: every caller passes a pointer to a live, initialized `#[repr(C)]` struct matching the
    // request's expected layout (UI_DEV_SETUP/UI_ABS_SETUP); the kernel reads it during the call
    // and retains nothing.
    if unsafe { libc::ioctl(fd, req, arg) } < 0 {
        bail!("{what}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// One evdev event in a pending frame — `(type, code, value)`, flushed together under one
/// `SYN_REPORT`.
type Ev = (u16, u16, i32);

/// A finger the compositor currently believes is down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Contact {
    /// The wire's finger id ([`InputEvent::code`]) — reusable by the client after its `TouchUp`.
    id: u32,
    /// The MT slot this finger owns until it lifts.
    slot: i32,
}

/// The wire-id → MT-slot table and the frame codec over it. Split from the device so the whole
/// protocol decision — which slot, when `BTN_TOUCH` flips, what a lift emits — is testable without
/// `/dev/uinput`, which no test environment can be assumed to have.
#[derive(Default)]
struct Contacts {
    live: Vec<Contact>,
    /// Monotonic MT tracking id. Never the wire id: the client reuses a finger id the moment that
    /// finger lifts, and MT-B tells a new touch from a continuing one by the tracking id in the
    /// slot changing.
    next_tracking_id: i32,
}

impl Contacts {
    /// The lowest slot no live contact owns.
    fn free_slot(&self) -> i32 {
        let mut slot = 0;
        while self.live.iter().any(|c| c.slot == slot) {
            slot += 1;
        }
        slot
    }

    /// Tracking ids stay non-negative (`-1` is MT-B's "lifted"), so wrap rather than overflow.
    fn take_tracking_id(&mut self) -> i32 {
        let id = self.next_tracking_id;
        self.next_tracking_id = if id == i32::MAX { 0 } else { id + 1 };
        id
    }

    /// Position events for a contact, plus the single-touch emulation axes when it is the PRIMARY
    /// (oldest live) finger — a real touchscreen reports `ABS_X`/`ABS_Y` for one finger, and the
    /// device declares them because that is what `input_id` classifies a touchscreen by.
    fn position(&self, slot: i32, x: i32, y: i32, out: &mut Vec<Ev>) {
        out.push((EV_ABS, ABS_MT_POSITION_X, x));
        out.push((EV_ABS, ABS_MT_POSITION_Y, y));
        if self.live.first().is_some_and(|c| c.slot == slot) {
            out.push((EV_ABS, ABS_X, x));
            out.push((EV_ABS, ABS_Y, y));
        }
    }

    /// Start tracking `id` at `(x, y)`: allocate its slot, open the MT tracking id, and flip
    /// `BTN_TOUCH` if this is the first finger down. `None` when every slot is taken — a drop,
    /// never an eviction: a live finger the compositor is tracking must not lose its slot to a
    /// newcomer.
    fn down(&mut self, id: u32, x: i32, y: i32) -> Option<Vec<Ev>> {
        if self.live.len() >= MAX_CONTACTS {
            return None;
        }
        let slot = self.free_slot();
        let tracking = self.take_tracking_id();
        let first = self.live.is_empty();
        self.live.push(Contact { id, slot });
        let mut out = vec![
            (EV_ABS, ABS_MT_SLOT, slot),
            (EV_ABS, ABS_MT_TRACKING_ID, tracking),
        ];
        self.position(slot, x, y, &mut out);
        if first {
            out.push((EV_KEY, BTN_TOUCH, 1));
        }
        Some(out)
    }

    /// Translate one wire touch event into the evdev frame that expresses it. `None` = nothing to
    /// send (an unusable extent, a lift for a finger we never tracked, or a down past the slot
    /// ceiling).
    fn frame(&mut self, ev: &InputEvent) -> Option<Vec<Ev>> {
        if ev.kind == InputKind::TouchUp {
            let i = self.live.iter().position(|c| c.id == ev.code)?;
            let slot = self.live.remove(i).slot;
            let mut out = vec![
                (EV_ABS, ABS_MT_SLOT, slot),
                (EV_ABS, ABS_MT_TRACKING_ID, -1),
            ];
            if self.live.is_empty() {
                out.push((EV_KEY, BTN_TOUCH, 0));
            }
            return Some(out);
        }
        // Down/move carry the client's touch surface in `flags`, exactly like `MouseMoveAbs`; a
        // zero extent is the same documented drop it is there (nothing to scale against).
        let (w, h) = ((ev.flags >> 16) & 0xffff, ev.flags & 0xffff);
        if w == 0 || h == 0 {
            return None;
        }
        let x = scale(ev.x, w);
        let y = scale(ev.y, h);
        match self.live.iter().position(|c| c.id == ev.code) {
            Some(i) => {
                let slot = self.live[i].slot;
                let mut out = vec![(EV_ABS, ABS_MT_SLOT, slot)];
                self.position(slot, x, y, &mut out);
                Some(out)
            }
            // A DOWN for a finger already lifted, or a MOVE whose DOWN was lost on the unreliable
            // datagram plane: begin the contact here so the stroke self-heals rather than being
            // invisible until the user lifts and touches again.
            None => self.down(ev.code, x, y),
        }
    }
}

/// Wire pixel → the declared 0..[`ABS_MAX`] axis, clamped to the surface (a client that reports a
/// touch a pixel outside its own content rect must not wrap around the output).
///
/// A zero extent is unreachable through [`Contacts::frame`], which drops such an event before
/// scaling; should it ever be reached, the origin is the harmless answer — dividing by a clamped
/// `1` instead would put every finger in the far corner.
fn scale(v: i32, extent: u32) -> i32 {
    if extent == 0 {
        return 0;
    }
    let extent = extent as i64;
    let v = (v as i64).clamp(0, extent);
    (v * ABS_MAX as i64 / extent) as i32
}

/// The virtual touchscreen, created lazily on the first wire touch (a session that never touches
/// never creates a device) and destroyed with the injector that owns it (Drop → `UI_DEV_DESTROY`,
/// after lifting whatever is still down).
pub struct VirtualTouchscreen {
    fd: OwnedFd,
    contacts: Contacts,
}

impl VirtualTouchscreen {
    pub fn create() -> Result<VirtualTouchscreen> {
        use std::os::fd::FromRawFd;
        // SAFETY: `c"/dev/uinput"` is a 'static NUL-terminated C string literal; `open` reads it as
        // a path, returns a fresh fd (or -1) and retains nothing.
        let raw = unsafe {
            libc::open(
                c"/dev/uinput".as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            bail!(
                "open /dev/uinput: {} (install the udev rule granting the 'input' group access \
                 — see scripts/60-punktfunk.rules — and add the user to the 'input' group)",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: `raw >= 0` here, a freshly-opened fd owned nowhere else; `OwnedFd` becomes the
        // unique owner and closes it exactly once on drop.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        ioctl_int(raw, UI_SET_EVBIT, EV_KEY as i32, "UI_SET_EVBIT(EV_KEY)")?;
        ioctl_int(raw, UI_SET_EVBIT, EV_ABS as i32, "UI_SET_EVBIT(EV_ABS)")?;
        // The ONLY key: `input_id` reads a touchscreen as "direct device with ABS_X/ABS_Y and
        // BTN_TOUCH but no stylus/finger tool keys" — declaring BTN_TOOL_FINGER here would make it
        // a touchPAD instead, and pointer motion is precisely what this exists to stop being.
        ioctl_int(raw, UI_SET_KEYBIT, BTN_TOUCH as i32, "UI_SET_KEYBIT")?;
        ioctl_int(
            raw,
            UI_SET_PROPBIT,
            INPUT_PROP_DIRECT,
            "UI_SET_PROPBIT(DIRECT)",
        )?;

        // Position spans the full u16 range on both the MT axes and the single-touch emulation
        // ones — the wire's coordinates are normalized, and the compositor maps that range onto
        // the output rect.
        //
        // `resolution` (units/mm) is left UNDECLARED on purpose, unlike the sibling pen's 100. The
        // physical size of the client's glass is not something the host can know (nor does it stay
        // still — a mid-session resize renegotiates the mode), and nothing on the touch path reads
        // it: libinput's mm math serves touchPAD gestures, which a direct device never runs.
        // Declaring one anyway is not free — it is what a device's size is COMPUTED from, so a
        // made-up number becomes a made-up `Size: 655x655mm` in `libinput list-devices` and a
        // made-up `ID_INPUT_WIDTH_MM` in udev, for anyone later debugging a touch problem.
        // Verified on-glass: with no resolution, udev still tags `ID_INPUT_TOUCHSCREEN=1` and
        // libinput still reports `Capabilities: touch` on seat0.
        let pos = AbsInfo {
            minimum: 0,
            maximum: ABS_MAX,
            ..Default::default()
        };
        for (code, info) in [
            (ABS_X, pos),
            (ABS_Y, pos),
            (ABS_MT_POSITION_X, pos),
            (ABS_MT_POSITION_Y, pos),
            (
                // One slot per simultaneous contact; the kernel sizes its slot table from this.
                ABS_MT_SLOT,
                AbsInfo {
                    minimum: 0,
                    maximum: MAX_CONTACTS as i32 - 1,
                    ..Default::default()
                },
            ),
            (
                ABS_MT_TRACKING_ID,
                AbsInfo {
                    minimum: 0,
                    maximum: i32::MAX,
                    ..Default::default()
                },
            ),
        ] {
            let mut a = UinputAbsSetup {
                code,
                _pad: 0,
                absinfo: info,
            };
            ioctl_ptr(raw, UI_ABS_SETUP, &mut a, "UI_ABS_SETUP")?;
        }

        // A stable, distinctive identity (pid.codes open-source VID, sibling product id to the
        // pen's `0x5046`) so a compositor's per-device mapping rule — sway's
        // `input "1209:5054:Punktfunk_Touchscreen" map_to_output` — can target exactly this
        // device instead of every touchscreen on the box.
        let mut setup = UinputSetup {
            id: InputId {
                bustype: 0x0006, // BUS_VIRTUAL
                vendor: 0x1209,
                product: 0x5054, // "PT"
                version: 1,
            },
            name: [0; 80],
            ff_effects_max: 0,
        };
        let name = b"Punktfunk Touchscreen";
        setup.name[..name.len()].copy_from_slice(name);
        ioctl_ptr(raw, UI_DEV_SETUP, &mut setup, "UI_DEV_SETUP")?;
        ioctl_int(raw, UI_DEV_CREATE, 0, "UI_DEV_CREATE")?;
        tracing::info!(
            slots = MAX_CONTACTS,
            "virtual touchscreen created (Punktfunk Touchscreen, uinput MT-B) — the compositor \
             maps it onto ONE output, so pin that to the streamed head in the compositor's own \
             config (niri: touch map-to-output, sway: input map_to_output)"
        );

        Ok(VirtualTouchscreen {
            fd,
            contacts: Contacts::default(),
        })
    }

    /// Apply one wire touch event: one event, one SYN frame.
    pub fn apply(&mut self, ev: &InputEvent) {
        if let Some(frame) = self.contacts.frame(ev) {
            self.write_frame(&frame);
        }
    }

    /// Write a whole frame — its events plus the closing `SYN_REPORT` — in ONE `write`. uinput
    /// takes a batch per call, and a single call is what makes the frame indivisible: a partial
    /// write cannot leave the kernel holding position updates with no SYN to publish them.
    fn write_frame(&self, frame: &[Ev]) {
        let syn: Ev = (EV_SYN, SYN_REPORT, 0);
        let mut buf: Vec<InputEventRaw> = Vec::with_capacity(frame.len() + 1);
        for &(type_, code, value) in frame.iter().chain(std::iter::once(&syn)) {
            buf.push(InputEventRaw {
                time: libc::timeval {
                    tv_sec: 0,
                    tv_usec: 0,
                },
                type_,
                code,
                value,
            });
        }
        // SAFETY: `buf` holds initialized `#[repr(C)]` all-integer structs (no padding: timeval=16
        // + u16 + u16 + i32 = 24), so every byte of the slice is initialized; it spans exactly
        // `buf`'s elements and is used immediately below with no concurrent mutation.
        let bytes = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr() as *const u8,
                std::mem::size_of_val(buf.as_slice()),
            )
        };
        // Best-effort like the pen/gamepad paths: a full kernel queue drops the frame, and the
        // next one re-states the slot's position (a lost lift is the one that hurts, which is why
        // Drop lifts everything still down).
        // SAFETY: `self.fd` stays open for the synchronous call; `write` only reads `bytes.len()`
        // bytes from the still-live buffer and retains nothing.
        let _ = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
            )
        };
    }
}

impl Drop for VirtualTouchscreen {
    fn drop(&mut self) {
        // Lift every finger still down BEFORE the device disappears. A compositor that learns of
        // the touchscreen's removal mid-stroke has to invent an ending for it; sending the real
        // one costs a single frame and leaves no window in which a window manager thinks a finger
        // is still on the glass.
        let held: Vec<i32> = self.contacts.live.iter().map(|c| c.slot).collect();
        if !held.is_empty() {
            let mut frame: Vec<Ev> = Vec::with_capacity(held.len() * 2 + 1);
            for slot in held {
                frame.push((EV_ABS, ABS_MT_SLOT, slot));
                frame.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
            }
            frame.push((EV_KEY, BTN_TOUCH, 0));
            self.write_frame(&frame);
        }
        // SAFETY: `self.fd` is still open (OwnedFd closes only after this body returns);
        // UI_DEV_DESTROY takes no pointer argument. Errors are moot on teardown.
        let _ = unsafe { libc::ioctl(self.fd.as_raw_fd(), UI_DEV_DESTROY, 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: InputKind, id: u32, x: i32, y: i32, w: u32, h: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code: id,
            x,
            y,
            flags: (w << 16) | h,
        }
    }

    fn down(id: u32, x: i32, y: i32) -> InputEvent {
        ev(InputKind::TouchDown, id, x, y, 1000, 1000)
    }

    fn mv(id: u32, x: i32, y: i32) -> InputEvent {
        ev(InputKind::TouchMove, id, x, y, 1000, 1000)
    }

    fn up(id: u32) -> InputEvent {
        // The wire sends only the id on a lift — no coordinates, no extent.
        ev(InputKind::TouchUp, id, 0, 0, 0, 0)
    }

    /// A one-finger stroke: the slot opens with a tracking id and `BTN_TOUCH`, moves carry
    /// position only, and the lift closes both.
    #[test]
    fn single_finger_stroke() {
        let mut c = Contacts::default();
        let f = c.frame(&down(7, 500, 250)).unwrap();
        assert_eq!(f[0], (EV_ABS, ABS_MT_SLOT, 0));
        assert_eq!(f[1], (EV_ABS, ABS_MT_TRACKING_ID, 0));
        assert!(f.contains(&(EV_ABS, ABS_MT_POSITION_X, ABS_MAX / 2)));
        assert!(f.contains(&(EV_ABS, ABS_MT_POSITION_Y, ABS_MAX / 4)));
        // The primary finger also drives the single-touch emulation axes.
        assert!(f.contains(&(EV_ABS, ABS_X, ABS_MAX / 2)));
        assert!(f.contains(&(EV_KEY, BTN_TOUCH, 1)));

        let f = c.frame(&mv(7, 1000, 1000)).unwrap();
        assert!(f.contains(&(EV_ABS, ABS_MT_POSITION_X, ABS_MAX)));
        assert!(!f.iter().any(|e| e.1 == ABS_MT_TRACKING_ID));
        assert!(!f.iter().any(|e| e.1 == BTN_TOUCH));

        let f = c.frame(&up(7)).unwrap();
        assert_eq!(f[0], (EV_ABS, ABS_MT_SLOT, 0));
        assert_eq!(f[1], (EV_ABS, ABS_MT_TRACKING_ID, -1));
        assert!(f.contains(&(EV_KEY, BTN_TOUCH, 0)));
        assert!(c.live.is_empty());
    }

    /// Two fingers get their own slots, and `BTN_TOUCH` belongs to the FIRST down and the LAST up
    /// — not to every transition (a pinch would otherwise flap it).
    #[test]
    fn second_finger_gets_its_own_slot_and_no_btn_touch() {
        let mut c = Contacts::default();
        c.frame(&down(1, 0, 0)).unwrap();
        let f = c.frame(&down(2, 500, 500)).unwrap();
        assert_eq!(f[0], (EV_ABS, ABS_MT_SLOT, 1));
        assert!(!f.iter().any(|e| e.1 == BTN_TOUCH));
        // Only the primary (slot 0) writes the emulation axes.
        assert!(!f.iter().any(|e| e.1 == ABS_X));

        let f = c.frame(&up(1)).unwrap();
        assert!(!f.iter().any(|e| e.1 == BTN_TOUCH)); // finger 2 is still down
        let f = c.frame(&up(2)).unwrap();
        assert!(f.contains(&(EV_KEY, BTN_TOUCH, 0)));
    }

    /// A lifted slot is reused, but with a FRESH tracking id — that is what tells the compositor
    /// this is a new touch rather than a continuing one.
    #[test]
    fn slot_is_reused_with_a_new_tracking_id() {
        let mut c = Contacts::default();
        c.frame(&down(1, 0, 0)).unwrap();
        c.frame(&up(1)).unwrap();
        let f = c.frame(&down(1, 0, 0)).unwrap();
        assert_eq!(f[0], (EV_ABS, ABS_MT_SLOT, 0));
        assert_eq!(f[1], (EV_ABS, ABS_MT_TRACKING_ID, 1));
    }

    /// A MOVE whose DOWN never arrived (datagram loss) begins the contact instead of vanishing.
    #[test]
    fn move_for_an_unknown_finger_self_heals() {
        let mut c = Contacts::default();
        let f = c.frame(&mv(3, 250, 0)).unwrap();
        assert_eq!(f[1], (EV_ABS, ABS_MT_TRACKING_ID, 0));
        assert!(f.contains(&(EV_KEY, BTN_TOUCH, 1)));
        assert_eq!(c.live.len(), 1);
    }

    /// A lift for a finger we never tracked, and a down with no usable surface, emit nothing.
    #[test]
    fn nothing_to_send_stays_nothing() {
        let mut c = Contacts::default();
        assert!(c.frame(&up(9)).is_none());
        assert!(c.frame(&ev(InputKind::TouchDown, 1, 5, 5, 0, 0)).is_none());
        assert!(c.live.is_empty());
    }

    /// Past the slot ceiling the newcomer is dropped — never a live finger, whose slot the
    /// compositor is still tracking.
    #[test]
    fn contacts_beyond_the_ceiling_are_dropped_not_evicted() {
        let mut c = Contacts::default();
        for id in 0..MAX_CONTACTS as u32 {
            assert!(c.frame(&down(id, 0, 0)).is_some());
        }
        assert!(c.frame(&down(99, 0, 0)).is_none());
        assert_eq!(c.live.len(), MAX_CONTACTS);
        assert!(c.live.iter().all(|x| x.id != 99));
    }

    /// The scale is the wire's own absolute contract: the surface's far edge is the axis maximum,
    /// and a sample outside it clamps instead of wrapping.
    #[test]
    fn scale_spans_the_surface_and_clamps() {
        assert_eq!(scale(0, 1000), 0);
        assert_eq!(scale(1000, 1000), ABS_MAX);
        assert_eq!(scale(-5, 1000), 0);
        assert_eq!(scale(4000, 1000), ABS_MAX);
        assert_eq!(scale(10, 0), 0); // extent guard: never a divide by zero
    }
}
