{ pkgs ? import
    (fetchTarball {
      # nixup: pin=jpetrucciani/nix;
      name = "jpetrucciani-2026-11-13";
      url = "https://github.com/jpetrucciani/nix/archive/7c182951459b46c656024aaae938648b806e8909.tar.gz";
      sha256 = "027vzb2mb4zvy9wzjnk4bdhl70z8x7r3zkcjzhh42if1wm3d7xp7";
    })
    { overlays = [ _rust ]; }
, _rust ? import
    (fetchTarball {
      name = "oxalica-2026-11-13";
      url = "https://github.com/oxalica/rust-overlay/archive/84edd48f141b0c8910dffdd0296ddd577e829132.tar.gz";
      sha256 = "0h8j7qz8wlf27cx32g5xbgsihpd3v5psnmz2yhlsjhjyrns4hsx1";
    })
}:
let
  name = "jet";
  winTarget = "x86_64-pc-windows-msvc";
  # jetmux's daemon, its terminal client and what reaches it over a Unix
  # socket; the protocol and roaming crates build everywhere (jet attaches
  # from Windows with them).
  jetmuxUnixOnly = [ "jetmux" "jetmux-client" "jetmux-core" "jetmux-mcp" "jetmux-notify" "jetmux-rec" "jetmux-render" "jetmux-web" ];
  rust = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default.override {
    # llvm-tools: an llvm-profdata on rustc's own LLVM, for pgo.
    extensions = [ "rust-src" "rustc-dev" "rust-analyzer" "llvm-tools" ];
    # musl and macOS: jetmux's static server binary and universal Mac build.
    targets = [ "x86_64-unknown-linux-gnu" winTarget jetmuxTarget "aarch64-apple-darwin" "x86_64-apple-darwin" ];
  });

  # must match GHOSTTY_COMMIT in libghostty-vt-sys's build.rs; bump both together.
  # pointing GHOSTTY_SOURCE_DIR here skips the per-target git clone in that build script.
  ghosttySrc = pkgs.fetchFromGitHub {
    owner = "ghostty-org";
    repo = "ghostty";
    rev = "a887df42c56f6de86c0fe6da9c4eeca37931e083";
    sha256 = "0z1m4b718bvjfz8q2i61486n4crrimn34d74yw4vkpm443jzm76m";
  };

  # jetmux, the multiplexer: zig's package cache for libghostty in the nix
  # sandbox (real directories, since zig resolves relative paths lexically),
  # the web UI jetmux-web embeds, and the packages.
  ghosttyZigDeps = pkgs.callPackage "${ghosttySrc}/build.zig.zon.nix" {
    name = "ghostty-zig-deps";
    linkFarm = name: entries: pkgs.runCommand name { } ''
      mkdir -p $out
      ${pkgs.lib.concatMapStringsSep "\n" (e: "cp -rL ${e.path} $out/${e.name}") entries}
    '';
  };
  jetmuxTarget = "x86_64-unknown-linux-musl";
  node = pkgs.nodejs_22;
  rustPlatform = pkgs.makeRustPlatform {
    cargo = rust;
    rustc = rust;
  };
  # The web UI jetmux-web embeds, built from its pnpm lockfile.
  webUi = pkgs.stdenvNoCC.mkDerivation (finalAttrs: {
    pname = "jetmux-web-ui";
    version = "0.1.0";
    src = pkgs.lib.fileset.toSource {
      root = ./crates/jetmux-web/web;
      fileset = pkgs.lib.fileset.difference ./crates/jetmux-web/web (pkgs.lib.fileset.unions [
        (pkgs.lib.fileset.maybeMissing ./crates/jetmux-web/web/node_modules)
        (pkgs.lib.fileset.maybeMissing ./crates/jetmux-web/web/dist)
      ]);
    };
    pnpmDeps = pkgs.fetchPnpmDeps {
      inherit (finalAttrs) pname version src;
      inherit (pkgs) pnpm;
      fetcherVersion = 4;
      hash = "sha256-gETgCnetGz4qCvrMCP4CVS3jIaLdi7OHQIuCS8LE//A=";
    };
    nativeBuildInputs = [ node pkgs.pnpm pkgs.pnpmConfigHook ];
    buildPhase = ''
      runHook preBuild
      pnpm run build
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      cp -r dist $out
      runHook postInstall
    '';
  });

  jetmux = rustPlatform.buildRustPackage {
    pname = "jetmux";
    version = "0.1.0";
    src = pkgs.lib.fileset.toSource {
      root = ./.;
      # The UI comes in prebuilt from webUi, without node_modules.
      fileset = pkgs.lib.fileset.difference
        # no .cargo/config.toml: servers get the baseline CPU, not jet's floor
        (pkgs.lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./clippy.toml ./crates ])
        (pkgs.lib.fileset.unions [
          (pkgs.lib.fileset.maybeMissing ./crates/jetmux-web/web/node_modules)
          (pkgs.lib.fileset.maybeMissing ./crates/jetmux-web/web/dist)
        ]);
    };
    cargoLock.lockFile = ./Cargo.lock;
    nativeBuildInputs = [ pkgs.zig_0_15 ];
    # zig is only here for the build script; keep its hooks out of the phases
    dontUseZigBuild = true;
    dontUseZigInstall = true;
    dontUseZigCheck = true;
    env = {
      GHOSTTY_SOURCE_DIR = ghosttySrc;
      GHOSTTY_ZIG_SYSTEM_DIR = ghosttyZigDeps;
    };
    cargoBuildFlags = [ "-p" "jetmux" "-p" "jetmux-web" "-p" "jetmux-notify" "-p" "jetmux-mcp" "-p" "jetmux-rec" ];
    preBuild = ''
      cp -r ${webUi} crates/jetmux-web/web/dist
      chmod -R u+w crates/jetmux-web/web/dist
    '';
    # jetmux's crates only: jet's tests want fonts, a GPU and a desktop
    cargoTestFlags = pkgs.lib.concatMap (c: [ "-p" c ]) jetmuxUnixOnly ++ [ "-p" "jetmux-proto" "-p" "jetmux-roam" ];
    meta.mainProgram = "jetmux";
  };
  # the toolchain's musl target links fully static on its own (self-contained
  # crt), so build that target with the normal toolchain instead of pkgsStatic
  jetmux-static = jetmux.overrideAttrs {
    pname = "jetmux-static";
    buildPhase = ''
      runHook preBuild
      cargo build -j "$NIX_BUILD_CORES" --offline --release -p jetmux --target ${jetmuxTarget}
      runHook postBuild
    '';
    doCheck = false;
    installPhase = ''
      runHook preInstall
      install -Dm755 target/${jetmuxTarget}/release/jetmux $out/bin/jetmux
      runHook postInstall
    '';
  };

  # Licenses cargo-about can't see, appended to its THIRD-PARTY-NOTICES:
  # libghostty-vt-sys statically links Ghostty's lib-vt with uucode, simdutf
  # and highway, and jet-font embeds JetBrains Mono. The dependency versions
  # are the ones ghosttySrc's build.zig.zon files pin; bump them with it.
  extraNotices =
    let
      uucode = "https://raw.githubusercontent.com/jacobsandlund/uucode/v0.2.0";
    in
    pkgs.runCommand "jet-extra-notices" { } ''
      section() {
        printf '================================================================================\n%s\n\nUsed by:\n  %s\n\n' "$1" "$2"
        cat "$3"
        printf '\n\n'
      }
      {
        section "Ghostty (MIT License)" "libghostty-vt-sys (https://github.com/ghostty-org/ghostty)" ${ghosttySrc}/LICENSE
        section "uucode (MIT License)" "libghostty-vt-sys (https://github.com/jacobsandlund/uucode)" ${pkgs.fetchurl {
          url = "${uucode}/LICENSE.md";
          sha256 = "0cszc29ih74jnxcig6bj6g9q0gwy3wqyjnd89ixlgqib2hg90bii";
        }}
        section "uucode: Bjoern Hoehrmann's UTF-8 decoder (MIT License)" "libghostty-vt-sys" ${pkgs.fetchurl {
          url = "${uucode}/licenses/LICENSE_Bjoern_Hoehrmann";
          sha256 = "1p96fv0flc9k1qrdaajxl5wd6jfiv29k6xmz2yldbaijx7n9q8fy";
        }}
        section "uucode: Unicode data (Unicode License v3)" "libghostty-vt-sys" ${pkgs.fetchurl {
          url = "${uucode}/licenses/LICENSE_unicode";
          sha256 = "0xaim57imx03w1184s6vj33i51ikai4wv2rfn8vwfw3808xmmnhy";
        }}
        section "simdutf 9.0.0 (MIT License)" "libghostty-vt-sys (https://github.com/simdutf/simdutf)" ${pkgs.fetchurl {
          url = "https://raw.githubusercontent.com/simdutf/simdutf/v9.0.0/LICENSE-MIT";
          sha256 = "0pzsjdjpq9ymardfih04pjaim0avk5zzwzv4ib0fzm1sw02br3gw";
        }}
        section "Highway (BSD 3-Clause License)" "libghostty-vt-sys (https://github.com/google/highway)" ${pkgs.fetchurl {
          url = "https://raw.githubusercontent.com/google/highway/66486a10623fa0d72fe91260f96c892e41aceb06/LICENSE-BSD3";
          sha256 = "1wkhckzlqlvd4a265qdrd6w2ad0n64k654wrrhyclhnddbi84pnj";
        }}
        section "JetBrains Mono (SIL Open Font License 1.1)" "jet-font (https://github.com/JetBrains/JetBrainsMono)" ${./crates/jet-font/fonts/OFL.txt}
        section "iTerm2-Color-Schemes (MIT License)" "jet-config's built-in themes (https://github.com/mbadolato/iTerm2-Color-Schemes)" ${pkgs.fetchurl {
          url = "https://raw.githubusercontent.com/mbadolato/iTerm2-Color-Schemes/494b809/LICENSE";
          sha256 = "14ni18g6q3s2mn5xb1zng7did0k04hq3kswrzx6lj9xm2j31cwr3";
        }}
      } >$out
    '';

  # newer ConPTY than Windows ships inbox; it repaints correctly after resize.
  # portable-pty loads conpty.dll from beside jet.exe when present, and
  # conpty.dll starts the OpenConsole.exe next to it.
  conpty = pkgs.fetchzip {
    name = "conpty-1.25.261109001";
    url = "https://www.nuget.org/api/v2/package/Microsoft.Windows.Console.ConPTY/1.25.261109001";
    extension = "zip";
    stripRoot = false;
    sha256 = "sha256-n2tL2XZQ6Kt5nEEduxwRPzd/LP2uqOAYUFefBjfc4DY=";
  };

  inherit (pkgs.stdenv.hostPlatform) isDarwin isLinux;

  setVersion = "version=$(cargo pkgid -p jet | sed 's/.*[#@]//')";

  # What release builds and pgo build: jet-replay rides along so the crates
  # it shares with jet hash (and so profile) the same.
  pgoPackages = "-p jet -p jet-cli -p jet-replay";

  # pgo's workloads, into $1: floods of plain text, short lines, 256 colors,
  # truecolor, and full-screen redraws.
  pgoWork = pkgs.writeShellScript "pgo-work" ''
    set -euo pipefail
    mkdir -p "$1"
    ${pkgs.gawk}/bin/awk -v dir="$1" 'BEGIN {
            srand(7)
            word = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor"
            n = split(word, w, " ")
            for (i = 0; i < 40000; i++) {
              line = ""
              for (j = 0; j < 18; j++) line = line w[int(rand() * n) + 1] " "
              print i ": " line > (dir "/plain")
              print "line " i > (dir "/short")
              c = ""
              for (j = 0; j < 12; j++) c = c sprintf("\033[38;5;%dm%s \033[1;48;5;%dm%s\033[0m ", int(rand() * 256), w[j % n + 1], int(rand() * 256), i)
              print c > (dir "/color")
              t = ""
              for (j = 0; j < 30; j++) t = t sprintf("\033[38;2;%d;%d;%dm#", int(rand() * 256), int(rand() * 256), int(rand() * 256))
              print t "\033[0m" > (dir "/truecolor")
            }
            for (f = 0; f < 600; f++) {
              s = "\033[?2026h\033[H"
              for (r = 1; r <= 40; r++) s = s sprintf("\033[%d;1H\033[7m %3d \033[27m %s\033[K", r, f + r, substr(word word, (f + r) % n + 1, 60))
              printf "%s\033[?2026l", s > (dir "/tui")
            }
          }'
  '';

  # zig asks xcrun for the macOS SDK, and the stdenv's xcbuild xcrun knows
  # none. zig 0.15 can't link against SDKs whose stubs list only arm64e
  # (26 and later), so it gets the 15 SDK; anything else goes to Apple's xcrun.
  xcrun = pkgs.writeShellScriptBin "xcrun" ''
    case " $* " in
      *" --show-sdk-path "*) echo "${pkgs.apple-sdk_15}/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk" ;;
      *) exec /usr/bin/xcrun "$@" ;;
    esac
  '';

  envVars = {
    NIXUP = "0.0.15";
    GHOSTTY_SOURCE_DIR = "${ghosttySrc}";
    JET_CONPTY_DIR = "${conpty}";
    # fixed fallback fonts for jet-replay and the goldens: symbols and braille,
    # CJK, then color emoji (monochrome for what it lacks). On Windows jet asks DirectWrite instead.
    JET_FALLBACK_FONTS = pkgs.lib.concatStringsSep ":" [
      "${pkgs.dejavu_fonts}/share/fonts/truetype/DejaVuSans.ttf"
      "${pkgs.wqy_microhei}/share/fonts/truetype/wqy-microhei.ttc"
      "${pkgs.noto-fonts-color-emoji}/share/fonts/noto/NotoColorEmoji.ttf"
      "${pkgs.noto-fonts-monochrome-emoji}/share/fonts/noto/NotoEmoji.ttf"
    ];
    # libghostty's CPU, matching .cargo/config.toml's (Linux builds Linux
    # and Windows for x86-64).
    LIBGHOSTTY_VT_SYS_CPU = if isDarwin then "apple_m1" else "x86_64_v3";
  } // pkgs.lib.optionalAttrs isLinux {
    # wgpu dlopens libvulkan; lavapipe is added (not substituted) so tests can
    # ask for the fallback adapter and render identically without a GPU.
    LD_LIBRARY_PATH = "${pkgs.vulkan-loader}/lib";
    VK_ADD_DRIVER_FILES = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.x86_64.json";
  };
  tools = with pkgs; {
    cli = [
      actionlint
      cargo-about
      curl
      jfmt
      jq
      nixup
      zip
    ] ++ lib.optionals isLinux [
      # dist_win: the MSI, and signing through Azure Trusted Signing
      jsign
      msitools
    ];
    node = [ node pkgs.pnpm ];
    rust = [
      cargo-xwin
      # jetmux's macOS build; the stock wrapper puts a newer zig first on
      # PATH, which libghostty rejects
      (cargo-zigbuild.override { zig = zig_0_15; })
      rust
      zig_0_15
      llvmPackages.lld
      llvmPackages.llvm
      llvmPackages.clang-unwrapped
    ];
    scripts = pkgs.lib.attrsets.attrValues scripts;
  };

  scripts = with pkgs; rec {
    check = pog {
      name = "check";
      description = "fmt, clippy and tests for the linux build";
      script = ''
        set -euo pipefail
        cargo fmt --all --check
        cargo clippy --all --benches --tests --examples --all-features -- -D warnings
        cargo test --all
      '';
    };
    build_win = pog {
      name = "build_win";
      description = "cross-compile every workspace binary for ${winTarget}";
      script = ''
        set -euo pipefail
        # zig ships no MSVC headers, so libghostty-vt's C++ deps (simdutf, highway)
        # build against the CRT + SDK that cargo-xwin downloads. ZIG_LIBC is global
        # to zig, so it is only set here and never for linux builds.
        xwin="''${XWIN_CACHE_DIR:-''${XDG_CACHE_HOME:-$HOME/.cache}/cargo-xwin}/xwin"
        libc_file="$PWD/target/zig-libc-msvc-x86_64.txt"
        mkdir -p target
        cat >"$libc_file" <<EOF
        include_dir=$xwin/sdk/include/ucrt
        sys_include_dir=$xwin/crt/include
        crt_dir=$xwin/sdk/lib/ucrt/x86_64
        msvc_lib_dir=$xwin/crt/lib/x86_64
        kernel32_lib_dir=$xwin/sdk/lib/um/x86_64
        gcc_dir=
        EOF
        # PGO: instrumented for pgo_win_bundle, or with a profile from $JET_PGO.
        pgo_args=()
        if [ -n "''${JET_PGO_GENERATE:-}" ]; then
          pgo_args=(--config "target.${winTarget}.rustflags=[\"-Cprofile-generate=raw\"]")
        elif [ -n "''${JET_PGO:-}" ]; then
          pgo_args=(--config "target.${winTarget}.rustflags=[\"-Cprofile-use=$JET_PGO\"]")
        fi
        ZIG_LIBC="$libc_file" cargo xwin build --target ${winTarget} --release ${pgoPackages} "''${pgo_args[@]}"
      '';
    };
    pgo = pog {
      name = "pgo";
      description = "profile jet's hot paths for profile-guided release builds: target/pgo/jet.profdata";
      script = ''
        set -euo pipefail
        # An instrumented jet-replay benches floods like a busy terminal sees:
        # parsing, snapshots and frame builds, which is where jet spends its
        # time. libghostty is Zig, which has no PGO; this covers jet's Rust.
        # Profiles match functions by symbol, which carries each crate's hash,
        # so this builds the same packages as the release build (the hashes
        # follow feature unification) on the target it runs on. Release builds
        # read the profile from $JET_PGO; the dist scripts set it.
        host=$(rustc -vV | sed -n 's/^host: //p')
        dir="$PWD/target/pgo"
        rm -rf "$dir/raw" "$dir/work"
        mkdir -p "$dir/raw"
        ${pgoWork} "$dir/work"
        cargo build --release ${pgoPackages} --target-dir target/pgo/build \
          --config "target.$host.rustflags=[\"-Cprofile-generate=$dir/raw\"]"
        export LLVM_PROFILE_FILE="$dir/raw/jet-%p.profraw"
        for f in "$dir"/work/*; do
          for size in 100x30 300x80; do
            target/pgo/build/release/jet-replay "$f" --bench --size "$size" --scrollback 10000 >/dev/null
          done
        done
        "$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata" merge -o "$dir/jet.profdata" "$dir/raw"
        green "$dir/jet.profdata"
      '';
    };
    pgo_win_bundle = pog {
      name = "pgo_win_bundle";
      description = "an instrumented jet-replay.exe, workloads and run.cmd for recording a windows profile: target/pgo/win-bundle";
      script = ''
        set -euo pipefail
        # Run run.cmd on Windows; it leaves raw/*.profraw beside itself. Put
        # those in target/pgo/win-raw and dist_win builds with them.
        dir="$PWD/target/pgo/win-bundle"
        rm -rf "$dir"
        mkdir -p "$dir"
        ${pgoWork} "$dir/work"
        CARGO_TARGET_DIR=target/pgo/win-build JET_PGO_GENERATE=1 ${build_win}/bin/build_win
        cp target/pgo/win-build/${winTarget}/release/jet-replay.exe "$dir/"
        printf '%s\r\n' \
          '@echo off' \
          'set LLVM_PROFILE_FILE=%~dp0raw\jet-%%m.profraw' \
          'for %%f in ("%~dp0work\*") do (' \
          '  "%~dp0jet-replay.exe" "%%f" --bench --size 100x30 --scrollback 10000 >nul || exit /b 1' \
          '  "%~dp0jet-replay.exe" "%%f" --bench --size 300x80 --scrollback 10000 >nul || exit /b 1' \
          ')' \
          'echo pgo: recorded' >"$dir/run.cmd"
        green "$dir"
      '';
    };
    check_win = pog {
      name = "check_win";
      description = "clippy the windows-only code against ${winTarget}";
      script = ''
        set -euo pipefail
        ${build_win}/bin/build_win
        ZIG_LIBC="$PWD/target/zig-libc-msvc-x86_64.txt" \
          cargo xwin clippy --target ${winTarget} --workspace --all-targets \
            ${pkgs.lib.concatMapStringsSep " " (c: "--exclude ${c}") jetmuxUnixOnly} -- -D warnings
      '';
    };
    bundle_mac = pog {
      name = "bundle_mac";
      description = "build jet.app (jet and jet-cli, ad-hoc signed) into target/release";
      script = ''
        set -euo pipefail
        [ "$(uname)" = Darwin ] || die "bundle_mac runs on macOS"
        ${build_portable}/bin/build_portable
        app=target/release/jet.app
        ${setVersion}
        rm -rf "$app"
        mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
        cp target/portable/release/jet target/portable/release/jet-cli "$app/Contents/MacOS/"
        if /usr/bin/otool -L "$app"/Contents/MacOS/* | grep /nix/store; then
          die "jet.app links dylibs from /nix/store"
        fi
        sed "s/@VERSION@/$version/g" crates/jet/assets/Info.plist >"$app/Contents/Info.plist"
        # Apple's tools, not nixpkgs': sips scales jet.png (1024px), iconutil packs it.
        iconset=$(mktemp -d)/jet.iconset
        mkdir -p "$iconset"
        for size in 16 32 128 256 512; do
          /usr/bin/sips -s format png -z "$size" "$size" crates/jet/assets/jet.png --out "$iconset/icon_''${size}x''${size}.png" >/dev/null
          double=$((size * 2))
          /usr/bin/sips -s format png -z "$double" "$double" crates/jet/assets/jet.png --out "$iconset/icon_''${size}x''${size}@2x.png" >/dev/null
        done
        /usr/bin/iconutil -c icns "$iconset" -o "$app/Contents/Resources/jet.icns"
        rm -rf "$(dirname "$iconset")"
        /usr/bin/codesign --force --sign - "$app"
        green "built $app ($version)"
      '';
    };
    notices = pog {
      name = "notices";
      description = "write the license notices for what a release build of jet and jet-cli includes";
      flags = [
        { name = "target"; description = "target triple whose dependencies to list (default: the host's)"; }
        { name = "output"; short = "o"; description = "file to write (default: THIRD-PARTY-NOTICES)"; }
        { name = "workspace"; bool = true; description = "every workspace crate's dependencies, for jetmux's binaries (default: jet's)"; }
      ];
      script = ''
        set -euo pipefail
        target="''${target:-$(rustc -vV | sed -n 's/^host: //p')}"
        output="''${output:-THIRD-PARTY-NOTICES}"
        # jet depends on jet-cli, so its graph covers both shipped binaries.
        # jetmux's five have no such root, so they take the whole workspace's.
        scope=(-m crates/jet/Cargo.toml)
        if [ -n "''${workspace:-}" ]; then scope=(--workspace); fi
        cargo about generate --locked --fail "''${scope[@]}" --target "$target" -o "$output" about.hbs
        cat ${extraNotices} >>"$output"
      '';
    };
    dist_mac = pog {
      name = "dist_mac";
      description = "zip jet.app for a release into target/dist";
      script = ''
        set -euo pipefail
        ${pgo}/bin/pgo
        JET_PGO="$PWD/target/pgo/jet.profdata" ${bundle_mac}/bin/bundle_mac
        ${setVersion}
        out="target/dist/jet-$version-macos-$(uname -m).zip"
        mkdir -p target/dist
        rm -f "$out"
        stage=$(mktemp -d)
        # ditto keeps the bundle's symlinks, modes and signature intact.
        /usr/bin/ditto target/release/jet.app "$stage/jet.app"
        cp LICENSE "$stage/"
        ${notices}/bin/notices -o "$stage/THIRD-PARTY-NOTICES"
        /usr/bin/ditto -c -k "$stage" "$out"
        rm -rf "$stage"
        green "$out"
      '';
    };
    build_portable = pog {
      name = "build_portable";
      description = "release-build jet and jet-cli linked against the host's own libc, into target/portable";
      script = ''
        set -euo pipefail
        # The dev shell's cc links against nixpkgs' glibc (linux) or libiconv
        # (macOS), so its binaries only run where that /nix/store exists. This
        # build links with the system's cc and libc instead, so the host's glibc
        # is the oldest one the binaries run on. rustc, zig and ghostty stay pinned.
        # On linux rustc's bundled lld is nix-wrapped and adds an rpath, so the
        # system's ld links instead. macOS links against Xcode's SDK: nixpkgs'
        # apple-sdk drops libiconv.
        # Its own target dir keeps cargo from reusing a binary the dev shell linked.
        [ -x /usr/bin/cc ] || die "build_portable links with /usr/bin/cc, which this host lacks"
        # Flags go in as config, which adds to .cargo/config.toml's (the CPU)
        # where RUSTFLAGS would replace them.
        host=$(rustc -vV | sed -n 's/^host: //p')
        flags=()
        ${pkgs.lib.optionalString isLinux "flags+=(\"-Clinker-features=-lld\")"}
        if [ -n "''${JET_PGO:-}" ]; then flags+=("-Cprofile-use=$JET_PGO"); fi
        list=""
        if [ ''${#flags[@]} -gt 0 ]; then list=$(printf '"%s",' "''${flags[@]}"); fi
        env -i HOME="$HOME" ''${CARGO_HOME:+CARGO_HOME="$CARGO_HOME"} ''${TERM:+TERM="$TERM"} \
          PATH="${pkgs.lib.makeBinPath ([ rust zig_0_15 ] ++ pkgs.lib.optional isDarwin xcrun)}:/usr/bin:/bin" \
          GHOSTTY_SOURCE_DIR="${ghosttySrc}" \
          LIBGHOSTTY_VT_SYS_CPU="$LIBGHOSTTY_VT_SYS_CPU" \
          ${pkgs.lib.optionalString isDarwin ''MACOSX_DEPLOYMENT_TARGET=14.0 SDKROOT="$(/usr/bin/xcrun --sdk macosx --show-sdk-path)"''} \
          cargo build --release --target-dir target/portable ${pgoPackages} \
          --config "target.$host.rustflags=[''${list%,}]"
      '';
    };
    dist_linux = pog {
      name = "dist_linux";
      description = "tar jet and jet-cli for a release into target/dist";
      script = ''
        set -euo pipefail
        [ "$(uname)" = Linux ] || die "dist_linux runs on linux"
        ${pgo}/bin/pgo
        JET_PGO="$PWD/target/pgo/jet.profdata" ${build_portable}/bin/build_portable
        ${setVersion}
        bin=target/portable/release
        for exe in jet jet-cli; do
          if readelf -l -d "$bin/$exe" | grep -E '/nix/store|R(UN)?PATH'; then
            die "$exe has a /nix/store interpreter or an rpath"
          fi
        done
        name="jet-$version-linux-$(uname -m)"
        rm -rf "target/dist/$name"
        mkdir -p "target/dist/$name"
        cp "$bin/jet" "$bin/jet-cli" LICENSE "target/dist/$name/"
        ${notices}/bin/notices -o "target/dist/$name/THIRD-PARTY-NOTICES"
        tar -C target/dist --owner=0 --group=0 -czf "target/dist/$name.tar.gz" "$name"
        green "target/dist/$name.tar.gz"
      '';
    };
    dist_jetmux = pog {
      name = "dist_jetmux";
      description = "tar jetmux's static binaries (any x86_64 linux, baseline cpu) for a release into target/dist";
      script = ''
        set -euo pipefail
        [ "$(uname)" = Linux ] || die "dist_jetmux runs on linux"
        ${jetmux_web_build}/bin/jetmux_web_build
        # A server binary runs on whatever box it's copied to: no -C target-cpu.
        unset LIBGHOSTTY_VT_SYS_CPU
        bins=(jetmux jetmux-web jetmux-notify jetmux-mcp jetmux-rec)
        cargo build --locked --release --target ${jetmuxTarget} "''${bins[@]/#/-p=}"
        ${setVersion}
        name="jetmux-$version-linux-x86_64"
        rm -rf "target/dist/$name"
        mkdir -p "target/dist/$name"
        for exe in "''${bins[@]}"; do
          if readelf -l "target/${jetmuxTarget}/release/$exe" | grep -q INTERP; then
            die "$exe isn't static"
          fi
          cp "target/${jetmuxTarget}/release/$exe" "target/dist/$name/"
        done
        cp docs/jetmux.toml LICENSE "target/dist/$name/"
        ${notices}/bin/notices --workspace --target ${jetmuxTarget} -o "target/dist/$name/THIRD-PARTY-NOTICES"
        tar -C target/dist --owner=0 --group=0 -czf "target/dist/$name.tar.gz" "$name"
        green "target/dist/$name.tar.gz"
      '';
    };
    dist_win = pog {
      name = "dist_win";
      description = "zip and MSI of jet.exe, jet-cli.exe and the bundled ConPTY for a release into target/dist, signed when JET_SIGN_TOKEN is set";
      script = ''
        set -euo pipefail
        command -v zip >/dev/null || die "dist_win needs zip on PATH"
        # Azure Trusted Signing, when there's a token: JET_SIGN_TOKEN (az
        # account get-access-token --resource https://codesigning.azure.net),
        # JET_SIGN_ENDPOINT (e.g. eus.codesigning.azure.net), JET_SIGN_ACCOUNT
        # and JET_SIGN_PROFILE. Unsigned otherwise.
        sign() {
          [ -n "''${JET_SIGN_TOKEN:-}" ] || return 0
          jsign --storetype TRUSTEDSIGNING --keystore "$JET_SIGN_ENDPOINT" \
            --storepass "$JET_SIGN_TOKEN" --alias "$JET_SIGN_ACCOUNT/$JET_SIGN_PROFILE" \
            --tsaurl http://timestamp.acs.microsoft.com --tsmode RFC3161 "$@"
        }
        [ -n "''${JET_SIGN_TOKEN:-}" ] || yellow "JET_SIGN_TOKEN unset: building unsigned"
        # A profile only fits the target it was recorded on: one from
        # pgo_win_bundle's run.cmd, its raw/ copied to target/pgo/win-raw.
        raw=target/pgo/win-raw
        if [ -n "$(find "$raw" -name '*.profraw' 2>/dev/null)" ]; then
          host=$(rustc -vV | sed -n 's/^host: //p')
          "$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata" merge -o target/pgo/win.profdata "$raw"
          JET_PGO="$PWD/target/pgo/win.profdata" ${build_win}/bin/build_win
        else
          yellow "no windows profile in $raw (see pgo_win_bundle): building without PGO"
          ${build_win}/bin/build_win
        fi
        ${setVersion}
        out="$PWD/target/dist/jet-$version-windows-x86_64.zip"
        stage=$(mktemp -d)
        cp target/${winTarget}/release/jet.exe target/${winTarget}/release/jet-cli.exe "$stage/"
        # conpty.dll and OpenConsole.exe must sit beside jet.exe, as deploy uploads them.
        cp "${conpty}/runtimes/win-x64/native/conpty.dll" "${conpty}/build/native/runtimes/x64/OpenConsole.exe" "$stage/"
        cp LICENSE "$stage/"
        ${notices}/bin/notices --target ${winTarget} -o "$stage/THIRD-PARTY-NOTICES"
        # The ConPTY package is Microsoft's, under the same MIT terms.
        sed 's/^Copyright (c) .*/Copyright (c) Microsoft Corporation. All rights reserved./' LICENSE >"$stage/LICENSE-conpty"
        chmod 644 "$stage"/*
        # conpty.dll and OpenConsole.exe carry Microsoft's signature already.
        sign "$stage/jet.exe" "$stage/jet-cli.exe"
        mkdir -p target/dist
        rm -f "$out"
        (cd "$stage" && zip -q -X "$out" ./*)
        msi="$PWD/target/dist/jet-$version-windows-x86_64.msi"
        wixl -a x64 -D Version="$version" -D Stage="$stage" -D Icon=crates/jet/assets/jet.ico \
          -o "$msi" packaging/jet.wxs
        sign "$msi"
        rm -rf "$stage"
        green "$out"
        green "$msi"
      '';
    };
    update_themes = pog {
      name = "update_themes";
      description = "bundle Ghostty's themes from an iTerm2-Color-Schemes release into crates/jet-config/themes.txt";
      flags = [
        { name = "release"; description = "the release tag, e.g. release-20261116-150709-494b809"; required = true; }
      ];
      script = ''
        set -euo pipefail
        tmp=$(mktemp -d)
        trap 'rm -rf "$tmp"' EXIT
        base="https://github.com/mbadolato/iTerm2-Color-Schemes/releases/download"
        curl -fsSL -o "$tmp/themes.tgz" "$base/$release/ghostty-themes.tgz"
        mkdir "$tmp/x"
        tar -xzf "$tmp/themes.tgz" -C "$tmp/x"
        out=crates/jet-config/themes.txt
        # One file, as some names (with ':') can't be file names on Windows.
        {
          echo "# Ghostty's themes, from iTerm2-Color-Schemes $release"
          echo "# (MIT License, Mark Badolato and contributors; the text is in"
          echo "# THIRD-PARTY-NOTICES). Each starts at its [name]. Made by update_themes."
          find "$tmp/x/ghostty" -type f -printf '%f\n' | LC_ALL=C sort | while IFS= read -r name; do
            printf '\n[%s]\n' "$name"
            grep -v '^[[:space:]]*$' "$tmp/x/ghostty/$name" || true
          done
        } >"$out"
        green "$(grep -c '^\[' "$out") themes in $out"
      '';
    };
    win_manifests = pog {
      name = "win_manifests";
      description = "scoop and winget manifests for the release in target/dist: target/dist/jet.json and target/winget";
      flags = [
        { name = "repo"; description = "owner/name on GitHub"; envVar = "GITHUB_REPOSITORY"; required = true; }
      ];
      script = ''
        set -euo pipefail
        ${setVersion}
        base="https://github.com/$repo/releases/download/v$version"
        zip="jet-$version-windows-x86_64.zip"
        msi="jet-$version-windows-x86_64.msi"
        sum() { sha256sum "target/dist/$1" | cut -d' ' -f1; }
        # Scoop installs straight from this file's URL (releases/latest/
        # download/jet.json), and `scoop update` rereads it there.
        jq -n --arg v "$version" --arg repo "$repo" --arg url "$base/$zip" --arg hash "$(sum "$zip")" '{
          version: $v,
          description: "A fast, native terminal on libghostty-vt",
          homepage: "https://github.com/\($repo)",
          license: "MIT",
          architecture: { "64bit": { url: $url, hash: $hash } },
          bin: ["jet.exe", "jet-cli.exe"],
          shortcuts: [["jet.exe", "jet"]],
          checkver: "github",
          autoupdate: { architecture: { "64bit": {
            url: "https://github.com/\($repo)/releases/download/v$version/jet-$version-windows-x86_64.zip"
          } } }
        }' >target/dist/jet.json
        # winget: the three files a winget-pkgs PR adds, from the MSI.
        id="''${repo%%/*}.jet"
        dir="target/winget/$id/$version"
        rm -rf target/winget
        mkdir -p "$dir"
        head="PackageIdentifier: $id"$'\n'"PackageVersion: $version"
        tail="ManifestVersion: 1.6.0"
        printf '%s\n' "$head" "DefaultLocale: en-US" "ManifestType: version" "$tail" >"$dir/$id.yaml"
        printf '%s\n' "$head" "InstallerType: wix" "Scope: user" "UpgradeBehavior: install" \
          "Installers:" "  - Architecture: x64" "    InstallerUrl: $base/$msi" \
          "    InstallerSha256: $(sum "$msi" | tr a-f A-F)" "ManifestType: installer" "$tail" \
          >"$dir/$id.installer.yaml"
        printf '%s\n' "$head" "PackageLocale: en-US" "Publisher: ''${repo%%/*}" "PackageName: jet" \
          "PackageUrl: https://github.com/$repo" "License: MIT" \
          "ShortDescription: A fast, native terminal on libghostty-vt" \
          "ManifestType: defaultLocale" "$tail" >"$dir/$id.locale.en-US.yaml"
        green target/dist/jet.json
        green "$dir"
      '';
    };
    deploy = pog {
      name = "deploy";
      description = "build jet.exe, upload it with the bundled ConPTY to jet-devhost and run it";
      flags = [
        { name = "host"; short = "H"; description = "jet-devhost base url, e.g. http://winbox:7878"; envVar = "JET_DEVHOST"; required = true; }
        { name = "tag"; description = "build dir name on the box (default: timestamp)"; }
      ];
      script = ''
        set -euo pipefail
        tag="''${tag:-$(date +%Y%m%d-%H%M%S)}"
        ${build_win}/bin/build_win
        exe="target/${winTarget}/release/jet.exe"
        blue "uploading $exe as $tag"
        curl -fsS -T "$exe" "$host/build/$tag/jet.exe"
        curl -fsS -T "${conpty}/runtimes/win-x64/native/conpty.dll" "$host/build/$tag/conpty.dll"
        curl -fsS -T "${conpty}/build/native/runtimes/x64/OpenConsole.exe" "$host/build/$tag/OpenConsole.exe"
        blue "running"
        curl -fsS -X POST -H 'content-type: application/json' \
          -d "{\"version\":\"$tag\",\"exe\":\"jet.exe\"}" "$host/run"
        echo
        sleep 2
        curl -fsS "$host/logs"
      '';
    };
    deploy_devhost = pog {
      name = "deploy_devhost";
      description = "build jet-devhost.exe and have the running devhost swap itself for it";
      flags = [
        { name = "host"; short = "H"; description = "jet-devhost base url, e.g. http://winbox:7878"; envVar = "JET_DEVHOST"; required = true; }
      ];
      script = ''
        set -euo pipefail
        ${build_win}/bin/build_win
        blue "updating devhost"
        curl -fsS -T "target/${winTarget}/release/jet-devhost.exe" "$host/self"
        # the old process answers for ~300ms before it exits
        sleep 1
        for _ in $(seq 1 50); do
          if curl -fsS -o /dev/null "$host/logs" 2>/dev/null; then
            green "devhost is back"
            exit 0
          fi
          sleep 0.2
        done
        die "devhost did not come back; check devhost.log on the box"
      '';
    };
    smoke_win = pog {
      name = "smoke_win";
      description = "deploy jet, type a command into it and save a screenshot of the result";
      flags = [
        { name = "host"; short = "H"; description = "jet-devhost base url, e.g. http://winbox:7878"; envVar = "JET_DEVHOST"; required = true; }
        { name = "text"; description = "command to type, enter is added (default: echo jet-ok)"; }
      ];
      script = ''
        set -euo pipefail
        ${deploy}/bin/deploy --host "$host"
        sleep 2
        payload=$(jq -cn --arg text "''${text:-echo jet-ok}" '{text: ($text + "\n")}')
        curl -fsS -X POST -H 'content-type: application/json' -d "$payload" "$host/input"
        sleep 1
        ${shot}/bin/shot --host "$host"
      '';
    };
    shot = pog {
      name = "shot";
      description = "save a screenshot from jet-devhost into shots/";
      flags = [
        { name = "host"; short = "H"; description = "jet-devhost base url, e.g. http://winbox:7878"; envVar = "JET_DEVHOST"; required = true; }
        { name = "desktop"; bool = true; description = "the whole desktop instead of the running build's window"; }
      ];
      script = ''
        set -euo pipefail
        mkdir -p shots
        out="shots/$(date +%Y%m%d-%H%M%S).png"
        query="?window=true"
        if [ -n "''${desktop:-}" ]; then query=""; fi
        curl -fsS -o "$out" "$host/screenshot$query"
        echo "$out"
      '';
    };
    jetmux_macos = pog {
      name = "jetmux_macos";
      description = "cross-build a universal (arm64 + x86_64) macOS binary of jetmux";
      script = ''
        cargo zigbuild --release -p jetmux --target universal2-apple-darwin
        ls -la target/universal2-apple-darwin/release/jetmux
      '';
    };
    jetmux_ts = pog {
      name = "jetmux_ts";
      description = "regenerate the web UI's TypeScript types from the Rust ones";
      script = ''
        TS_RS_EXPORT_DIR="$PWD/crates/jetmux-web/web/src/proto" \
          cargo test -p jetmux-web --features ts export_bindings
      '';
    };
    jetmux_web_build = pog {
      name = "jetmux_web_build";
      description = "build the web UI that jetmux-web embeds";
      script = ''
        cd crates/jetmux-web/web || exit 1
        pnpm install --frozen-lockfile
        pnpm run build
      '';
    };
    check_web = pog {
      name = "check_web";
      description = "the web UI's TypeScript types match the Rust ones, and it typechecks and builds";
      script = ''
        set -euo pipefail
        proto=crates/jetmux-web/web/src/proto
        before=$(cat "$proto"/*.ts | sha256sum)
        ${jetmux_ts}/bin/jetmux_ts
        if [ "$(cat "$proto"/*.ts | sha256sum)" != "$before" ]; then
          git diff --stat -- "$proto" || true
          die "$proto is stale: run jetmux_ts and keep the result"
        fi
        ${jetmux_web_build}/bin/jetmux_web_build
      '';
    };
    # runs against a throwaway daemon with no config; the numbers are the
    # spec's perf targets minus input latency
    jetmux_perf = pog {
      name = "jetmux_perf";
      description = "measure jetmux's throughput, attach time and idle memory per pane";
      flags = [
        { name = "bin"; description = "jetmux binary to measure (default: a fresh release build)"; }
        { name = "size"; default = "1024"; description = "MB to cat through a pane"; }
        { name = "runs"; default = "20"; description = "attaches to time"; }
        { name = "panes"; default = "50"; description = "idle panes to add for the memory check"; }
      ];
      script = ''
        if [ -z "$bin" ]; then
          cargo build --release -p jetmux
          bin=target/release/jetmux
        fi
        bin=$(realpath "$bin")
        dir=$(mktemp -d /tmp/jetmux-perf.XXXXXX)
        export XDG_RUNTIME_DIR=$dir JETMUX_CONFIG=$dir/none.toml
        unset JETMUX JETMUX_SESSION JETMUX_PANE
        jetmux() { "$bin" -L perf "$@"; }
        cleanup() {
          jetmux kill-server 2>/dev/null
          rm -rf "$dir"
        }
        trap cleanup EXIT
        now() { ${python3}/bin/python3 -c 'import time; print(time.monotonic())'; }

        yes 'the quick brown fox jumps over the lazy dog 0123456789 THE QUICK BROWN FOX' |
          head -c "$((size * 1024 * 1024))" >"$dir/data"
        ${python3}/bin/python3 ${ptyCeiling} "$dir/data"
        start=$(now)
        jetmux new -d -s cat -- sh -c "cat '$dir/data'; echo PERF_DONE; exec sleep 100000" >/dev/null
        jetmux pane wait -t cat --timeout 600 PERF_DONE >/dev/null
        end=$(now)
        awk -v mb="$size" -v s="$start" -v e="$end" \
          'BEGIN { printf "throughput  %d MB in %.2fs (%.0f MB/s, detached)\n", mb, e - s, mb / (e - s) }'
        jetmux new -d -s drawn -- sh -c "echo PERF_READY; read _; cat '$dir/data'; echo PERF_DONE; exec sleep 100000" >/dev/null
        ${python3}/bin/python3 ${attachedThroughput} "$bin" perf drawn "$size"
        jetmux kill -t drawn

        jetmux new -d -s attach -- sh -c 'echo PERF_MARK; exec sleep 100000' >/dev/null
        jetmux pane wait -t attach PERF_MARK >/dev/null
        ${python3}/bin/python3 ${attachTime} "$bin" perf attach "$runs"

        jetmux kill -t cat
        jetmux kill -t attach
        jetmux new -d -s rss -- sleep 100000 >/dev/null
        pid=$(${procps}/bin/pgrep -f -- "-L perf daemon")
        sleep 1
        before=$(${procps}/bin/ps -o rss= -p "$pid")
        for _ in $(seq "$panes"); do
          jetmux tab new -t rss -- sleep 100000 >/dev/null
        done
        sleep 1
        after=$(${procps}/bin/ps -o rss= -p "$pid")
        awk -v n="$panes" -v b="$before" -v a="$after" \
          'BEGIN { printf "idle rss    %d KB daemon, +%d KB for %d panes (%.0f KB/pane)\n", b, a - b, n, (a - b) / n }'
      '';
    };
  };
  # how fast this machine's pty moves data to a reader that does nothing, as a
  # ceiling for jetmux's throughput
  ptyCeiling = pkgs.writeText "jetmux-pty-ceiling.py" ''
    import os, pty, sys, time

    start = time.perf_counter()
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp("cat", ["cat", sys.argv[1]])
    total = 0
    try:
        while chunk := os.read(fd, 65536):
            total += len(chunk)
    except OSError:
        pass
    os.waitpid(pid, 0)
    secs = time.perf_counter() - start
    print(f"raw pty     {total >> 20} MB in {secs:.2f}s ({(total >> 20) / secs:.0f} MB/s, no terminal)")
  '';
  # the throughput run again with a client attached and drawing into a pty
  # that is drained like a terminal would
  attachedThroughput = pkgs.writeText "jetmux-attached-throughput.py" ''
    import fcntl, os, pty, select, struct, subprocess, sys, termios, threading, time

    jetmux, server, session, mb = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))
        os.execv(jetmux, [jetmux, "-L", server, "attach", "-t", session])
    seen = b""
    while b"PERF_READY" not in seen:
        if not select.select([fd], [], [], 5)[0]:
            sys.exit("attach: no screen after 5s")
        seen += os.read(fd, 65536)
    drawn = 0

    def drain():
        global drawn
        try:
            while chunk := os.read(fd, 65536):
                drawn += len(chunk)
        except OSError:
            pass

    drainer = threading.Thread(target=drain)
    drainer.start()
    run = lambda *args: subprocess.run([jetmux, "-L", server, *args], check=True, capture_output=True)
    start = time.perf_counter()
    run("pane", "send-keys", "-t", session, "Enter")
    run("pane", "wait", "-t", session, "--timeout", "600", "PERF_DONE")
    secs = time.perf_counter() - start
    os.write(fd, b"\x02d")
    drainer.join()
    os.waitpid(pid, 0)
    print(f"attached    {mb} MB in {secs:.2f}s ({mb / secs:.0f} MB/s, client drew {drawn >> 20} MB at 200x50)")
  '';
  # times `jetmux attach` from exec until the pane's text reaches the terminal,
  # then detaches with the default prefix
  attachTime = pkgs.writeText "jetmux-attach-time.py" ''
    import fcntl, os, pty, select, statistics, struct, sys, termios, time

    jetmux, server, session, runs = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    times = []
    for _ in range(runs):
        start = time.perf_counter()
        pid, fd = pty.fork()
        if pid == 0:
            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
            os.execv(jetmux, [jetmux, "-L", server, "attach", "-t", session])
        seen = b""
        while b"PERF_MARK" not in seen:
            if not select.select([fd], [], [], 5)[0]:
                sys.exit("attach: no screen after 5s")
            seen += os.read(fd, 65536)
        times.append((time.perf_counter() - start) * 1000)
        os.write(fd, b"\x02d")
        try:
            while os.read(fd, 65536):
                pass
        except OSError:
            pass
        os.waitpid(pid, 0)
    print(f"attach      median {statistics.median(times):.1f} ms, best {min(times):.1f} ms ({runs} runs)")
  '';
  paths = pkgs.lib.flatten [ (builtins.attrValues tools) ];
  env = pkgs.buildEnv {
    inherit name paths; buildInputs = paths;
  };
in
(env.overrideAttrs (old: {
  inherit name;
  env = (old.env or { }) // envVars;
  shellHook = pkgs.lib.optionalString isDarwin ''
    export PATH="${xcrun}/bin:$PATH"
  '';
})) // {
  inherit scripts jetmux jetmux-static;
  homeManagerModules.default = import ./home-manager.nix { inherit jetmux; };
}
