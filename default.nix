{
  lib,
  rustPlatform,
  makeWrapper,
  nix,
  nix-eval-jobs,
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

  # systemctl comes from the host systemd.
  postInstall = ''
    wrapProgram $out/bin/flakelet \
      --prefix PATH : ${
        lib.makeBinPath [
          nix
          nix-eval-jobs
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
