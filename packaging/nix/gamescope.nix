# `punktfunk-gamescope` — nixpkgs' gamescope carrying punktfunk's `pipewire-hdr` patches, exposed
# under its own name so it sits BESIDE the system gamescope instead of replacing it.
#
# An override rather than a from-scratch derivation on purpose: gamescope vendors wlroots,
# vkroots, libliftoff, libdisplay-info, SPIRV-Headers and reshade as git submodules plus two meson
# wraps, and nixpkgs already solves all of that. What we add is the patch set and a rename.
#
# This is also why the override needs none of the `force_fallback_for` armour
# `build-punktfunk-gamescope.sh` carries: that exists because meson silently prefers a SYSTEM
# wlroots when the build host has one and links it shared, producing a binary that starts only on
# machines with that dev library. A nix closure names every library it links, so the outcome is
# whatever nixpkgs' own gamescope already does — reproducibly.
#
# `gamescope` in nixpkgs is a wrapper (it wires the WSI layer + capabilities); the buildable
# derivation is `gamescope.unwrapped` — patching the wrapper would be a no-op, so this asserts on
# it rather than silently shipping an unpatched binary.
#
# Version drift: the patches are applied to whatever gamescope your nixpkgs pins, NOT to the
# commit `packaging/gamescope/build-punktfunk-gamescope.sh` names. Both hunks sit in code that has
# been stable across the 3.16 series (`src/pipewire.cpp`'s format builders, `paint_pipewire()` in
# `src/steamcompmgr.cpp`), so this normally just works — and when it does not, the build fails
# loudly at `patchPhase` rather than producing a gamescope that quietly cannot do HDR.
{
  lib,
  gamescope,
  patchDir,
}:
let
  # As of nixos-unstable (checked 2026-07-28) `gamescope` IS the buildable derivation — pname
  # "gamescope", version 3.16.25, carrying `src`/`patches`/`mesonFlags`. Revisions that wrap it
  # (to wire the WSI layer + capabilities) expose the build as `.unwrapped`, so prefer that where
  # it exists and take `gamescope` itself otherwise.
  #
  # The check is on the RESULT, not on which attribute we found: `overrideAttrs` on a symlinkJoin
  # wrapper succeeds and does nothing, which would hand us an UNPATCHED gamescope installed under
  # our own name — the single worst outcome here, because the host reads the name as a promise of
  # HDR. (`installCheckPhase` below greps for the marker as the second line of defence; this one
  # fails at eval, before anything is built.)
  base = gamescope.unwrapped or gamescope;
  unwrapped =
    if base ? src then
      base
    else
      throw ''
        punktfunk-gamescope needs a buildable gamescope derivation (one with a `src` that
        `overrideAttrs` can patch); this nixpkgs' `gamescope` is neither that nor a wrapper
        exposing `.unwrapped`. Update nixpkgs, or build the compositor with
        packaging/gamescope/build-punktfunk-gamescope.sh instead.
      '';
in
unwrapped.overrideAttrs (old: {
  pname = "punktfunk-gamescope";

  # Read the patch DIRECTORY rather than naming files: `builtins.attrNames` sorts
  # lexicographically, which for `000N-` prefixes is exactly the apply order, and a patch added or
  # renamed upstream of this file can no longer leave nix silently building a subset. (That already
  # happened once — this list still named the level-1 banner patch after level 2 landed, so a nix
  # build would have shipped a binary with no cursor patch and a marker claiming otherwise.)
  patches =
    (old.patches or [ ])
    ++ map (f: "${patchDir}/${f}") (
      builtins.filter (lib.hasSuffix ".patch") (builtins.attrNames (builtins.readDir patchDir))
    );

  # nixpkgs builds from a `fetchFromGitHub` src, so there is no `.git` for `git describe` and the
  # banner would read `+pfhdr2 (gcc …)` with no version at all — which the host's diagnostic
  # version gate then misreads (it takes the first X.Y.Z triple it finds, i.e. the compiler's).
  # Substituting the real version in keeps `--version` honest AND keeps our marker.
  postPatch = (old.postPatch or "") + ''
    substituteInPlace src/meson.build \
      --replace-fail \
        "vcs_tag = run_command(vcs_tag_cmd, check: false).stdout().strip()" \
        "vcs_tag = '${old.version}'"
  '';

  # Expose the compositor under our own name, ADDITIVELY — a symlink beside nixpkgs' own layout
  # rather than a rename plus a sweep of everything else.
  #
  # The sweep this replaces (`find $out ! -name bin -exec rm -rf {} +`, `find $out/bin ! -name
  # gamescope -delete`, `mv gamescope punktfunk-gamescope`) assumed a plain meson install, the way
  # the Arch PKGBUILD gets one. Against nixpkgs' recipe it breaks three ways, all caused by what
  # nixpkgs' OWN postInstall does immediately before ours runs:
  #
  #   1. It copies the ReShade shaders out of the store (`cp -r ${frogShaders}/* …`), which
  #      preserves mode 555 on the directories — so the `rm -rf` fails with a pile of
  #      `Permission denied` and takes the build down with it.
  #   2. It runs `wrapProgram $out/bin/gamescope`, leaving the real ELF at `.gamescope-wrapped`;
  #      the `-delete` sweep removes exactly that, so the renamed wrapper would exec a file that
  #      no longer exists (and `installCheckPhase` below would catch it).
  #   3. It bakes an absolute `$out/bin/gamescopereaper` into the binary (`substituteInPlace
  #      src/Utils/Process.cpp --subst-var-by "gamescopereaper" …`). The reaper is what launches
  #      the nested `-- <app>`, i.e. the host's bare-spawn path — deleting it produces a package
  #      that builds fine and fails at stream time.
  #
  # Keeping nixpkgs' output intact costs nothing here: this package lands on the punktfunk-host
  # unit's PATH, not a user profile, and `gamescope_bin()` looks for `punktfunk-gamescope` first,
  # so the plain `gamescope` name sitting next to it is never picked by accident.
  postInstall = (old.postInstall or "") + ''
    ln -s gamescope $out/bin/punktfunk-gamescope
  '';

  # `gamescope --version` exits non-zero on some builds; the grep is the real assertion.
  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    $out/bin/punktfunk-gamescope --version 2>&1 | grep -q '+pfhdr' \
      || { echo "punktfunk-gamescope: the +pfhdr marker is missing — the patches did not take"; exit 1; }
    runHook postInstallCheck
  '';

  meta = (old.meta or { }) // {
    description = "gamescope with 10-bit BT.2020/PQ PipeWire capture, for punktfunk HDR streaming";
    mainProgram = "punktfunk-gamescope";
  };
})
