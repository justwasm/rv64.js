{
  description = "rv64.js — RISC-V emulator in Rust/wasm that boots Linux in the browser";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rust = pkgs.rust-bin.stable."1.97.1".default.override {
          targets = [
            "wasm32-unknown-unknown"
            "riscv64gc-unknown-linux-musl"
          ];
        };
        riscvGcc = pkgs.pkgsCross.riscv64-embedded.buildPackages.gcc;
        spike = pkgs.spike.overrideAttrs (old: {
          configureFlags = (old.configureFlags or [ ]) ++ [ "--enable-commitlog" ];
        });

        linux726Source = pkgs.fetchurl {
          url = "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.2.6.tar.xz";
          hash = "sha256-A5rvhPKwmUrto/T8/D0C7J16m7uQIOomTEP0Rshg9gY=";
        };
        linux726 = crossPkgs:
          (crossPkgs.callPackage
            "${pkgs.path}/pkgs/os-specific/linux/kernel/generic.nix"
            { }
          ) {
            version = "7.2.6";
            src = linux726Source;
            ignoreConfigErrors = false;
          };
        riscvLinux = linux726 pkgs.pkgsCross.riscv64;
        arm64Linux = linux726 pkgs.pkgsCross.aarch64-multiplatform;
        x86Linux = linux726 pkgs.pkgsCross.gnu32;

        kernelImage = { linux, config, imagePath, patch ? "", target ? null }:
          (linux.override ({
            defconfig = "allnoconfig";
            enableCommonConfig = false;
            autoModules = false;
            preferBuiltin = true;
            structuredExtraConfig = import config {
              inherit (pkgs) lib;
            };
            ignoreConfigErrors = false;
          } // pkgs.lib.optionalAttrs (target != null) {
            inherit target;
          })).overrideAttrs (old: {
            postPatch = (old.postPatch or "") + patch;
            postInstall = ''
              cp ${imagePath} $out/Image
              mkdir -p "$modules"
            '';
          });

        rv64Patch = ''
          sed -i '/select VDSO_GETRANDOM if HAVE_GENERIC_VDSO && 64BIT/d' \
            arch/riscv/Kconfig
        '';
        rv64Kernel = kernelImage {
          linux = riscvLinux;
          config = ./kernel/rv64-config.nix;
          imagePath = "arch/riscv/boot/Image";
          patch = rv64Patch;
          target = "Image.gz";
        };
        rv64ContainerKernel = kernelImage {
          linux = riscvLinux;
          config = ./kernel/rv64-container-config.nix;
          imagePath = "arch/riscv/boot/Image";
          patch = rv64Patch;
          target = "Image.gz";
        };
        arm64Kernel = kernelImage {
          linux = arm64Linux;
          config = ./kernel/arm64-config.nix;
          imagePath = "arch/arm64/boot/Image";
        };
        arm64ContainerKernel = kernelImage {
          linux = arm64Linux;
          config = ./kernel/arm64-container-config.nix;
          imagePath = "arch/arm64/boot/Image";
        };
        x86Kernel = (kernelImage {
          linux = x86Linux;
          config = ./kernel/x86-v86-config.nix;
          imagePath = "arch/x86/boot/bzImage";
        }).overrideAttrs (old: {
          postInstall = ''
            cp arch/x86/boot/bzImage $out/bzImage
            mkdir -p "$modules"
          '';
        });
        x86ContainerKernel = (kernelImage {
          linux = x86Linux;
          config = ./kernel/x86-v86-container-config.nix;
          imagePath = "arch/x86/boot/bzImage";
        }).overrideAttrs (old: {
          postInstall = ''
            cp arch/x86/boot/bzImage $out/bzImage
            mkdir -p "$modules"
          '';
        });

        virtKernel = riscvLinux.override {
          structuredExtraConfig = with pkgs.lib.kernel; {
            VIRTIO = yes;
            VIRTIO_MMIO = yes;
            VIRTIO_BLK = yes;
            VIRTIO_NET = yes;
            VIRTIO_CONSOLE = yes;
            EXT4_FS = yes;
            PACKET = yes;
            NET_9P = yes;
            NET_9P_VIRTIO = yes;
            "9P_FS" = yes;
          };
          ignoreConfigErrors = false;
        };
        virtOpensbi = pkgs.pkgsCross.riscv64.opensbi;

        # Arch Linux bootstrap rootfs recipes. The script
          # integrations/wanix/build-linux-bundle.sh unpacks one of these
          # outputs in place of the alpine rootfs when WANIX_ROOTFS=arch,
          # then layers the kernel, init, wexec, hostexport, and any
          # profile-specific packages on top. Per-arch recipes live below:
          #   - riscv64 ships a prebuilt rootfs tarball that we fetch and
          #     unpack;
          #   - aarch64 ships its own prebuilt rootfs tarball from
          #     archlinuxarm (same fetch + unpack path);
          #   - i686 has no upstream bootstrap tarball, so we run pacstrap
          #     inside the build sandbox against the ufscar mirror and
          #     package the result. The matching pacman mirrorlist and
          #     pacman.conf live as plain text in
          #     integrations/wanix/arch-configs/ so they stay diff-friendly.
          archBootstrap = { url, sha256, format ? "zst" }:
          pkgs.stdenvNoCC.mkDerivation {
          name = "arch-bootstrap-${baseNameOf url}";
          src = pkgs.fetchurl { inherit url sha256; };
          nativeBuildInputs = [ pkgs.zstd ];
          dontUnpack = true;
          installPhase = ''
          runHook preInstall
          mkdir -p "$out"
          case "${format}" in
            zst)
            ${pkgs.zstd}/bin/zstd -d -c "$src" | ${pkgs.gnutar}/bin/tar -xf - -C "$out" --no-same-owner
            ;;
            gz)
            ${pkgs.gnutar}/bin/tar -xzf "$src" -C "$out" --no-same-owner
            ;;
            *)
            echo "unsupported arch bootstrap format: ${format}" >&2
            exit 2
            ;;
          esac
          runHook postInstall
          '';
          };

          archBootstrap_riscv64 = archBootstrap {
            url = "https://riscv.mirror.pkgbuild.com/images/archriscv-2026-08-27.tar.zst";
            sha256 = "a2045c8b62232db2f60d8e4db610dbb5d9e12856dab0ba08634ad3d7cb7ad498";
            };
            archBootstrap_aarch64 = archBootstrap {
            url = "https://ca.us.mirror.archlinuxarm.org/os/ArchLinuxARM-aarch64-latest.tar.gz";
            sha256 = "42a4eeaa038994ffd31fa173256ef2f0ef511358eeb41b9ea1f8626391b9b319";
            };
          # i686 has no published bootstrap tarball. Run pacstrap under
          # qemu-user-i386-static inside the build sandbox against the
          # ufscar mirror. The recipe stays opt-in: builds that do not
          # need i686 simply never reference `archBootstrap_i686`.
          archBootstrap_i686 = pkgs.runCommand "arch-bootstrap-i686" {
          nativeBuildInputs = [
            pkgs.qemu_user
            pkgs.pkgsi686Linux.pacstrap
          ];
          } ''
          mkdir -p "$out"
          # The sandbox already provides binfmt for i386; force the
          # interpreter so pacstrap does not try to invoke itself under
          # the host dynamic linker. arch-install-scripts' pacstrap
          # honours -G (copy host gpg keyring) and -M (no mirrorlist
          # copy) so we can inject our own /etc/pacman.d/mirrorlist via
          # the build's $pacman_bootstrap_conf below.
          ${pkgs.qemu_user}/bin/qemu-i386-static \
            -L ${pkgs.pkgsi686Linux.stdenv} \
            -E PATH=${pkgs.pkgsi686Linux.bash}/bin:${pkgs.pkgsi686Linux.coreutils}/bin:${pkgs.pkgsi686Linux.gnused}/bin \
            ${pkgs.pkgsi686Linux.pacstrap}/bin/pacstrap \
            -G -M -C ${./integrations/wanix/arch-configs/pacman-bootstrap.conf} \
            -K "$out" base >/dev/null
          '';

          archRecipe = pkgs.runCommand "wanix-linux-arch-recipe" { } ''
            mkdir -p "$out"
            cp -R ${archBootstrap_riscv64}   "$out/riscv64"
            cp -R ${archBootstrap_aarch64}   "$out/aarch64"
            mkdir -p "$out/etc"
            cp ${./integrations/wanix/arch-configs/mirrorlist} "$out/etc/mirrorlist"
            cp ${./integrations/wanix/arch-configs/mirrorlist.riscv64} "$out/etc/mirrorlist.riscv64"
            cp ${./integrations/wanix/arch-configs/pacman.conf} "$out/etc/pacman.conf"
            cp ${./integrations/wanix/arch-configs/pacman-bootstrap.conf} "$out/etc/pacman-bootstrap.conf"
            '';
            # i686 rootfs is built via pacstrap (no upstream tarball);
            # consumers opt in explicitly so the riscv64 + aarch64 aggregate
            # does not pull pkgsi686Linux into the build sandbox.
            archRecipeWithI686 = pkgs.runCommand "wanix-linux-arch-recipe-i686" { } ''
            mkdir -p "$out"
            cp -R ${archBootstrap_riscv64}   "$out/riscv64"
            cp -R ${archBootstrap_i686}      "$out/i686"
            cp -R ${archBootstrap_aarch64}   "$out/aarch64"
            mkdir -p "$out/etc"
            cp ${./integrations/wanix/arch-configs/mirrorlist} "$out/etc/mirrorlist"
            cp ${./integrations/wanix/arch-configs/mirrorlist.riscv64} "$out/etc/mirrorlist.riscv64"
            cp ${./integrations/wanix/arch-configs/mirrorlist.i686} "$out/etc/mirrorlist.i686"
            cp ${./integrations/wanix/arch-configs/pacman.conf} "$out/etc/pacman.conf"
            cp ${./integrations/wanix/arch-configs/pacman-bootstrap.conf} "$out/etc/pacman-bootstrap.conf"
            '';
          in
          {
          packages.virt-kernel = virtKernel;
          packages.virt-kernel-fast = rv64Kernel;
          packages.virt-kernel-fast-container = rv64ContainerKernel;
          packages.virt-opensbi = virtOpensbi;
          packages.arm64-kernel = arm64Kernel;
          packages.arm64-kernel-container = arm64ContainerKernel;
          packages.v86-kernel = x86Kernel;
          packages.v86-kernel-container = x86ContainerKernel;
          packages.arch-bootstrap-riscv64 = archBootstrap_riscv64;
            packages.arch-bootstrap-aarch64 = archBootstrap_aarch64;
            packages.arch-bootstrap-i686 = archBootstrap_i686;
            packages.arch-recipe = archRecipe;
            packages.arch-recipe-i686 = archRecipeWithI686;

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rust
            gcc
            gnumake
            autoconf
            automake
            python3
            curl
            git
            nodejs_20
            qemu
            spike
            dtc
            wabt
            binaryen
            riscvGcc
            cpio
            e2fsprogs
            util-linux
            zstd
            debootstrap
            apk-tools
            fakeroot
            dpkg
            gnutar
            gzip
            gnused
            wget
            pacman
            arch-install-scripts
          ];

          shellHook = ''
            export RISCV_PREFIX=riscv64-none-elf-
            echo "rv64.js dev shell — run tests/run-all.sh for the full suite"
          '';
        };
      });
}
