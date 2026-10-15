# Publishing Higuma and Koguma to Orchard

A plan for publishing Higuma (kuma-term) and Koguma (kuma-files) to
[Orchard](https://github.com/Letdown2491/orchard), a permissionless Flatpak
store: publishers push deterministic OCI artifacts to their own registry and
sign Nostr events that pin the artifact's digest; the client verifies bytes
against the pin and installs through flatpak. `PUBLISHING.md` there is the
publisher guide this plan assumes.

kuma-shell is not a candidate: it needs PAM, logind, the polkit system bus,
and niri's layer surfaces and IPC, none of which a sandbox grants. Its
distribution channel is the kumaOS image.

## Findings this plan stands on

- **Toolchain vs runtime.** Both binaries require `GLIBC_2.43` (built in the
  Fedora 44 dev container). The freedesktop 25.08 runtime carries glibc 2.42,
  so today's binaries would die in the sandbox. A 26.08 runtime exists, but
  Orchard's own publisher targets 25.08 and each listing pins one runtime per
  asset, so this plan pins the toolchain to Fedora 43 and targets 25.08.
- **Dependency survey.** Higuma: 18 shared-object deps, all shipped by the
  freedesktop runtime; zero bundling. Koguma: a 237-object closure whose tail
  beyond the runtime is libavcodec/format/util (.62/.60) plus codec and
  protocol deps (aom, dav1d, SvtAv1Enc, bluray, aribb24, aribcaption) and the
  samba private libraries that libavformat's smb protocol drags in. The
  assemble step bundles that closure into `files/lib` (flatpak puts `/app/lib`
  on the library path). gpui's Vulkan loader is dlopen'd; mesa in the runtime
  plus `--device=dri` covers rendering.
- **Repo facts.** kumaUI is public (the proof file and raw icon URLs
  resolve), MIT licensed, has 256px PNG icons in packaging (usable as listing
  icons; a 512px re-export is optional), and has no GitHub workflows yet, so
  the publish workflow is also kumaUI's first CI. Desktop file ids today are
  `kuma-files` and `kuma-term` (Name=Koguma and Name=Higuma). Kamuy
  (`kuma-monitor`) stays out of this plan until it graduates its dock week;
  its entry joins then as Name=Kamuy.
- **Secrets.** `ORCHARD_NSEC` exists as a secret in the orchard repo; it must
  also be added to `Letdown2491/kumaUI` for a kumaUI workflow to read it.
  Ghcr credentials ride on `GITHUB_TOKEN` with `github.actor` as the user.

## Decisions

- App ids `io.github.Letdown2491.Higuma` and `io.github.Letdown2491.Koguma`;
  the packaging desktop files are renamed to match, so the image's copy and
  the flatpak share one launcher id on kumaOS (the directory-handler binding
  ships the rename with it).
- One publisher key signs both apps. The nsec was generated offline and lives
  only in GitHub secrets; it never passes through an agent session or shell
  history.
- Registry: `ghcr.io/letdown2491/higuma` and `ghcr.io/letdown2491/koguma`,
  flipped public after the first push (ghcr packages are private by default).
- Runtime `org.freedesktop.Platform 25.08`, x86_64 only at first
  (`platforms = ["linux-x86_64"]`; the client shows an honest no-aarch64
  note). An aarch64 leg wants an arm build of the dev image on the arm
  runner; deferred.
- Sandbox for both apps: wayland socket, dri device, `filesystem=host`. For
  Koguma that is mandatory (it manages files); for Higuma it is a choice, and
  the client's permissions viewer shows it verbatim.
- Versions are read per app from each crate's Cargo.toml at build time
  (kuma-term can be 0.2.0 while the tag says v0.3.0; the two are not
  flattened).
- Channel split: the image stays the source of truth on kumaOS; Orchard is
  the distribution channel for every other desktop.

## Work, in order

1. Prerequisites: commit the Fedora 43 containerfile chain (the gcc base
   image currently exists only as a local image), add `ORCHARD_NSEC` to
   kumaUI's secrets.
2. Higuma end to end: the publish manifest, the assemble step (bin, export,
   metadata ini), the desktop-file and icon renames, and the workflow:
   checkout kumaUI and the orchard repo at a pinned SHA, build
   `orchard-publish` via orchard's build.sh, build the app via kumaUI's
   build.sh, assemble, push and sign with `--prove`, attach the oci.tgz to
   the release. Dry run with `--skip-relays` against a local registry, then a
   real tag.
3. Koguma: the libav-closure bundling step (ldd closure minus what the
   runtime ships), the sandbox metadata with `filesystem=host`, same
   workflow shape.
4. On the next release tag: real publish, flip the ghcr packages public,
   `curl` the proof file (`raw.githubusercontent.com/<owner>/<repo>/HEAD/orchard.txt`),
   verify in the Orchard client, and leave a NIP-46-signed review to exercise
   that flow.

## Known costs

- Two container builds per publish (kuma-dev-rust and orchard's dev image);
  both are cacheable.
- The 25.08 EOL treadmill: when the runtime ages out, rebuild on the
  toolchain matching the next runtime.
- The samba library tail rides along unless a slimmer libav is built later.
