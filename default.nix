{
  lib,
  rustPlatform,
  makeWrapper,
  nix,
  nix-eval-jobs,
  gnutar,
  zstd,
  getent,
}:

rustPlatform.buildRustPackage {
  pname = "flakelet";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./flakelet
      ./flakelet-core
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];

  # systemctl and chown come from the host.
  postInstall = ''
    wrapProgram $out/bin/flakelet \
      --prefix PATH : ${
        lib.makeBinPath [
          nix
          nix-eval-jobs
          gnutar
          zstd
          getent
        ]
      }
  '';

  meta = {
    description = "Runtime-managed systemd services from Nix flakes";
    license = lib.licenses.mit;
    mainProgram = "flakelet";
    platforms = lib.platforms.linux;
  };
}
