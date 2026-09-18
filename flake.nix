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
