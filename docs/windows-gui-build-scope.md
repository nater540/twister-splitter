# Scope: Windows GUI build in Docker + code signing

Author: scoping pass, 2026-07-09.

**Status: Part A (GUI build) BUILT & validated 2026-07-09.** `Dockerfile.windows`
now cross-compiles a single `twister-splitter.exe` (the GUI-capable dispatcher —
opens the GUI on double-click) via `cargo-xwin` (MSVC), `crt-static`, zero C deps,
no VC++ Redistributable — the build asserts self-containedness and passes. MinGW
retired; the redundant `twister-gui.exe` is not shipped on Windows (the CLI is
unused there). See `CLAUDE.md` → "Cross-compiling for Windows". **Part B (code
signing) is deferred** — the user opted to skip it for now; nothing signed.

Goal: produce a **GUI-enabled** Windows x86_64 build of `twister-splitter`,
cross-compiled **inside Docker** (no Windows host required for the build), and
lay out what it takes to ship it **without the "Unknown Publisher" prompt**.

This reverses one existing decision on purpose: today `Dockerfile.windows`
builds `--no-default-features`, which drops the `gui` feature to keep the exe a
self-contained static CLI (see `CLAUDE.md` → "Cross-compiling for Windows"). The
CLI exe stays; this adds a second, GUI-enabled artifact.

---

## TL;DR / recommendation

- **egui/eframe on Windows is fully supported** — this was never an egui
  limitation, only a packaging choice. The current CLI-only exe is by design, not
  because the GUI can't run there.
- **DECISION (2026-07-09): go MSVC via `cargo-xwin`, retire MinGW.** The user
  prefers the native MSVC target and is willing to drop the single-portable-exe
  property. So **unify both binaries (CLI + GUI) on `cargo-xwin`/MSVC** and delete
  the MinGW path (`Dockerfile.windows` gnu build + the `.cargo/config.toml`
  `x86_64-pc-windows-gnu` block). One toolchain, one Dockerfile.
- **crt-static bonus:** build with `-C target-feature=+crt-static` and even the
  MSVC exes stay **single dependency-free files (no VCRedist)** — portability kept
  for free. Fallback if a GUI dep rejects static CRT: dynamic CRT + ship VCRedist
  (the tradeoff the user already accepted). The old MinGW Path 1 below is kept for
  the record only; **Path 2 is the plan.**
- **Signing reality check:** signing removes "Unknown Publisher" and shows your
  name in the UAC/SmartScreen dialog, but **no cert type gives instant SmartScreen
  reputation anymore** (EV lost that in 2024). The blue "Windows protected your
  PC" screen only goes away as reputation accrues. Cheapest modern path:
  **Azure Artifact Signing (formerly Trusted Signing) ≈ $10/mo, signed from Linux
  with `jsign`.**

---

# Part A — GUI build in Docker

## A.0 The prerequisite both paths share: restructure GUI deps in `Cargo.toml`

The current optional GUI deps are Linux-shaped and **will not cross-compile to
Windows as written**:

```toml
eframe = { version = "0.35", default-features = false,
           features = ["default_fonts", "glow", "x11", "wayland"], optional = true }
rfd = { version = "0.17", optional = true }   # default features => gtk3
```

- **`rfd` pulls `gtk3` by default.** Cargo features are **not** target-gated, so
  cross-compiling to Windows still tries to build the `gtk`/`gtk-sys` C bindings
  → hard fail (no GTK in the MinGW/xwin sysroot). rfd already has a native Win32
  backend that needs **no** feature; on Linux it can use `xdg-portal` (no C) or
  `gtk3`.
- **`x11` / `wayland`** are Linux windowing backends; they're noise on Windows and
  `wayland` drags `wayland-sys`.

**Fix — make GUI deps target-specific:**

```toml
[dependencies]
eframe = { version = "0.35", default-features = false,
           features = ["default_fonts", "glow"], optional = true }
rfd    = { version = "0.17", default-features = false, optional = true }

# Linux-only extras (native builds / `cargo run` on macOS-Linux dev boxes)
[target.'cfg(all(unix, not(target_os = "macos")))'.dependencies]
eframe = { version = "0.35", default-features = false,
           features = ["default_fonts", "glow", "x11", "wayland"], optional = true }
rfd    = { version = "0.17", default-features = false, features = ["xdg-portal", "tokio"], optional = true }
```

(Exact table layout to be validated against Cargo's optional-dep + target-table
rules during the spike; the point is: **Windows gets glow + Win32 rfd with zero C
deps; Linux keeps x11/wayland + a portal/GTK file dialog.**) This is the crux of
why the GUI build was dropped in the first place — solve it once and both
toolchains below just work.

`default_fonts` + `glow` is the right renderer choice: `glow` (OpenGL) links
only the Windows system `opengl32.dll`; the 2D canvas needs nothing heavier than
`wgpu`/DX12 would add.

## A.1 Path 1 (recommended first): extend the existing MinGW Dockerfile

Smallest change. Keeps the `x86_64-pc-windows-gnu` toolchain and the
single-static-exe property already documented.

**Dockerfile delta** (new stage / target alongside the CLI one):

```dockerfile
# ... same builder base, mingw-w64, rustup target add x86_64-pc-windows-gnu ...

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --bin twister-gui --target x86_64-pc-windows-gnu \
 && cp target/x86_64-pc-windows-gnu/release/twister-gui.exe /twister-gui.exe
# note: NO --no-default-features here — we WANT the gui feature.
```

**Keep the self-contained assertion, widen its allow-list.** The GUI exe will now
import Windows **system** DLLs (`opengl32.dll`, `gdi32.dll`, `user32.dll`,
`comdlg32.dll`, `shell32.dll`, …) — all present on every Windows 10 box, so the
"no third-party DLLs" guarantee still holds. The existing check already only
*fails* on the MinGW runtime trio, so it needs no change to stay correct:

```dockerfile
RUN x86_64-w64-mingw32-objdump -p /twister-gui.exe | grep "DLL Name" \
 && ! x86_64-w64-mingw32-objdump -p /twister-gui.exe \
      | grep -qiE "libgcc|libstdc\+\+|libwinpthread"
```

`.cargo/config.toml` already sets `-C link-arg=-static` for this triple (statically
links libgcc/libstdc++/libwinpthread) — no change.

**Risks specific to MinGW:**
- Some crates in the eframe graph are primarily tested against **MSVC**; a
  `windows-gnu` link error in a transitive dep is the main thing that could push
  us to Path 2. Unknown until the spike runs.
- OpenGL context creation via `glutin` on `-gnu` is well-trodden but occasionally
  needs a linker nudge (`-lopengl32`); cheap to fix if it surfaces.

**Effort:** ~half a day *if* deps cooperate after A.0; the Cargo.toml restructure
is the bulk.

## A.2 Path 2 (fallback / "more native"): `cargo-xwin`, MSVC target

`cargo-xwin` cross-compiles to `x86_64-pc-windows-msvc` from Linux by
auto-downloading the MSVC CRT + Windows SDK headers/libs via `xwin` and linking
with `lld-link`. There's a ready-made Docker image with Rust + cargo-xwin + wine
preinstalled (`messense/cargo-xwin`).

```dockerfile
FROM messense/cargo-xwin AS builder
WORKDIR /app
COPY . .
RUN rustup target add x86_64-pc-windows-msvc \
 && cargo xwin build --release --bin twister-gui --target x86_64-pc-windows-msvc
```

**Why you might prefer it:**
- MSVC ABI is what Windows software normally ships; best crate compatibility, and
  no MinGW C-runtime question at all.
- `wgpu`/DX12 path is first-class if you ever move off `glow`.

**Costs / caveats:**
- New tooling + `xwin` downloads Microsoft SDK artifacts (you accept the Windows
  SDK license; `xwin` automates the EULA). Fine for a personal/CI build; note it
  for redistribution.
- The MSVC CRT is **not** statically linkable the way MinGW's is by default —
  the exe may depend on the **Visual C++ Redistributable** (`vcruntime140.dll`).
  Options: target the `*-static` CRT via `+crt-static` (bundles it, biggest exe,
  simplest for users) or ship the VCRedist. This **changes the "one dependency-free
  exe" story** — decide which matters more: native MSVC vs a single portable file.

**Effort:** ~a day (new image + validate `+crt-static` + the A.0 restructure).

## A.3 Testing the GUI build in Docker

- **Build-time:** the objdump import check (Path 1) is the cheap gate.
- **Smoke-run:** `cargo-xwin`'s image ships **wine**; `wine target/.../twister-gui.exe`
  can prove it *launches* and the CLI dispatch works. **But** an OpenGL GUI under
  wine needs a software GL stack (mesa/llvmpipe) and is flaky — treat a wine launch
  as "didn't instantly crash," **not** as visual QA.
- **Real QA belongs on Windows.** Recommend a **GitHub Actions `windows-latest`
  runner** as a second CI job that builds natively (`cargo build --bin twister-gui`)
  and, ideally, runs the `egui_kittest` snapshot tests (already a dev-dep). Docker
  gets you the shippable artifact; the Windows runner gets you confidence.

## A.4 Recommended phasing for Part A

1. **A.0 dep restructure** + prove `cargo build --bin twister-gui` still works on
   macOS/Linux (no regression to the dev GUI).
2. **Path 1 spike:** add the MinGW GUI stage, run `build-windows.sh`, check imports.
3. If MinGW links clean → done; wire it into `build-windows.sh` as a second
   artifact (`--target export` copying both exes) and update `CLAUDE.md`.
4. If MinGW fights → **Path 2** (`cargo-xwin`), decide `+crt-static` vs VCRedist.
5. Add a `windows-latest` CI job for real GUI QA.

---

# Part B — Shipping without the "Unknown Publisher" prompt

## B.1 What signing actually buys you (set expectations)

Two *different* Windows prompts are in play:

| Prompt | Trigger | What signing does |
|---|---|---|
| **UAC "Unknown Publisher"** (yellow) | elevation of an unsigned exe | **Fixed by signing** — shows your verified name instead of "Unknown Publisher". |
| **SmartScreen "Windows protected your PC"** (blue) | downloaded exe with no *reputation* | **Not fixed by signing alone** — needs reputation to accrue. |

**The key 2024 change:** EV code-signing certificates used to grant **instant**
SmartScreen reputation. **They no longer do.** Since 2024 Microsoft treats
reputation as a property of the file/publisher/cert built up over downloads —
"several weeks and hundreds of clean installs from a wide audience" — regardless
of OV vs EV. So **paying a premium for EV purely to dodge SmartScreen is no
longer justified.**

**Honest conclusion:** you can eliminate "Unknown Publisher" and show a trusted
publisher name today, cheaply. You **cannot** guarantee a zero-warning first-run
download for a brand-new low-volume binary — that comes with reputation over
time, or by sidestepping the download path (see B.5).

## B.2 The 2023 constraint that shapes "signing in Docker"

Since **June 2023** the CA/Browser Forum requires all publicly-trusted
code-signing **private keys to live on FIPS-140 hardware** (HSM, USB token, or a
cloud KMS). **You can no longer bake a `.pfx` into the Docker image** for a
publicly trusted cert. So "sign inside the pipeline" now means: the build runs in
Docker/CI and calls out to a **cloud-held key** to produce the signature.

## B.3 Certificate options (2026)

| Option | ~Cost | Key storage | SmartScreen | Notes |
|---|---|---|---|---|
| **Azure Artifact Signing** (was Trusted Signing) | **≈ $9.99/mo** | Microsoft-managed (cloud) | reputation-based (no instant trust) | Cheapest; **now open to self-employed individuals (US/Canada), no 3-yr-history rule as of Apr 2026**; signs from Linux via `jsign`. **Recommended.** |
| **OV cert + cloud HSM** (e.g. Azure Key Vault, SSL.com) | ~$200–400/yr + HSM | cloud KMS / token | reputation-based | Traditional; more setup. |
| **EV cert** | ~$300–600/yr | hardware token / cloud HSM | reputation-based (no longer instant) | **No SmartScreen advantage anymore** — not worth the premium for this. |

## B.4 Signing from Linux/Docker — toolchain

All three run on Linux, so they slot into the Docker/CI stage after the build:

- **`jsign`** (Java, cross-platform) — **best fit.** Native support for **Azure
  Trusted/Artifact Signing**, Azure Key Vault, AWS/GCP KMS, and PKCS#11 tokens.
  One tool covers whichever cert path you pick.
- **`AzureSignTool`** — drop-in `signtool` replacement specifically for **Azure
  Key Vault**-held certs.
- **`osslsigncode`** — OpenSSL-based Authenticode signer; use with a **PKCS#11**
  engine for hardware/HSM keys.

**Always add an RFC-3161 timestamp** (all three support `-t`/`--tsaurl`) so
signatures stay valid after the cert expires.

**Sketch (Azure Artifact Signing via jsign), as a Docker/CI step:**

```bash
# Secrets come from CI/Key Vault, never baked into the image.
jsign \
  --storetype TRUSTEDSIGNING \
  --keystore weu.codesigning.azure.net \        # your Trusted Signing account endpoint
  --storepass "$AZURE_ACCESS_TOKEN" \
  --alias   "MyAccount/MyCertProfile" \
  --tsaurl  http://timestamp.acs.microsoft.com \
  dist/twister-gui.exe
```

Because the signature covers the final PE, **sign after the build/objdump check**
— either as the last step of the Docker build (with the token passed as a build
secret, `--mount=type=secret`) or, cleaner, as a **separate CI step** after the
artifact is exported. Signing does not disturb the self-contained property.

## B.5 If you genuinely need zero warnings on day one

Signing + patience (reputation) is the mainstream answer. If a first-run
zero-warning experience is a hard requirement, the realistic levers are:

- **Ship through the Microsoft Store (MSIX).** Store-delivered apps bypass the
  SmartScreen download gauntlet. Biggest process change.
- **Submit the signed binary to Microsoft** for malware analysis / reputation
  seeding (helps, doesn't instantly guarantee).
- **Accept the warning and document it** for users ("More info → Run anyway") —
  legitimate for a low-volume internal/hobby tool, and free.

## B.6 Recommended phasing for Part B

1. **Sign with any cert to kill "Unknown Publisher"** — get a named publisher in
   the UAC dialog. Start an **Azure Artifact Signing** account (~$10/mo,
   individual-eligible).
2. **Wire `jsign` into the pipeline** as a post-build CI step against that key,
   with timestamping.
3. **Let reputation build** across releases; don't buy EV expecting a shortcut.
4. Revisit **MSIX/Store** only if a zero-warning first run becomes a hard req.

---

## Open decisions for you

1. **Artifact story:** keep "one portable dependency-free exe" (favors **MinGW**,
   or MSVC **+crt-static**) — or accept a VCRedist dependency for a native MSVC
   build? This drives Path 1 vs Path 2.
2. **Two exes or one:** ship the GUI exe *alongside* the existing CLI exe (both
   from Docker), or replace it? (The GUI binary already dispatches to the CLI when
   run with args, so one GUI exe could cover both — but that abandons the lean CLI.)
3. **Signing budget/identity:** individual vs org for Azure Artifact Signing; is
   ~$10/mo acceptable, and do you have a US/Canada individual/entity to enroll?
4. **Zero-warning hard requirement?** If yes, MSIX/Store enters scope; if "named
   publisher + warnings fade over time" is acceptable, B.1–B.4 is the whole job.

---

## Sources

- egui/eframe cross-platform (Windows native): <https://github.com/emilk/egui>,
  <https://crates.io/crates/eframe>, <https://docs.rs/eframe/latest/eframe/>
- `cargo-xwin` (MSVC-from-Linux, Docker image): <https://github.com/rust-cross/cargo-xwin>,
  <https://jake-shadle.github.io/xwin/>
- SmartScreen reputation model / EV no longer instant (2024+):
  <https://learn.microsoft.com/en-us/windows/apps/package-and-deploy/smartscreen-reputation>,
  <https://knowledge.digicert.com/alerts/ev-signed-application-showing-microsoft-defender-smartscreen-warnings>
- Azure Artifact/Trusted Signing (pricing, individual eligibility):
  <https://azure.microsoft.com/en-us/products/artifact-signing>,
  <https://learn.microsoft.com/en-us/azure/artifact-signing/faq>
- Sign from Linux: `jsign` <https://ebourg.github.io/jsign/>, `osslsigncode`
  <https://github.com/mtrojnar/osslsigncode>
