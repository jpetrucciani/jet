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
  rust = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.default.override {
    # llvm-tools: an llvm-profdata on rustc's own LLVM, for pgo.
    extensions = [ "rust-src" "rustc-dev" "rust-analyzer" "llvm-tools" ];
    targets = [ "x86_64-unknown-linux-gnu" winTarget ];
  });

  # must match GHOSTTY_COMMIT in libghostty-vt-sys's build.rs; bump both together.
  # pointing GHOSTTY_SOURCE_DIR here skips the per-target git clone in that build script.
  ghosttySrc = pkgs.fetchFromGitHub {
    owner = "ghostty-org";
    repo = "ghostty";
    rev = "a887df42c56f6de86c0fe6da9c4eeca37931e083";
    sha256 = "0z1m4b718bvjfz8q2i61486n4crrimn34d74yw4vkpm443jzm76m";
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
    ];
    rust = [
      cargo-xwin
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
          cargo xwin clippy --target ${winTarget} --workspace --all-targets -- -D warnings
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
      ];
      script = ''
        set -euo pipefail
        target="''${target:-$(rustc -vV | sed -n 's/^host: //p')}"
        output="''${output:-THIRD-PARTY-NOTICES}"
        # jet depends on jet-cli, so its graph covers both shipped binaries.
        cargo about generate --locked --fail -m crates/jet/Cargo.toml --target "$target" -o "$output" about.hbs
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
    dist_win = pog {
      name = "dist_win";
      description = "zip jet.exe, jet-cli.exe and the bundled ConPTY for a release into target/dist";
      script = ''
        set -euo pipefail
        command -v zip >/dev/null || die "dist_win needs zip on PATH"
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
        mkdir -p target/dist
        rm -f "$out"
        (cd "$stage" && zip -q -X "$out" ./*)
        rm -rf "$stage"
        green "$out"
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
  };
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
})) // { inherit scripts; }
