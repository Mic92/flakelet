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

  src = lib.cleanSource ./.;
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];

  # portablectl/systemctl/runuser come from the host systemd/util-linux.
  postInstall = ''
    wrapProgram $out/bin/flakelet \
      --prefix PATH : ${lib.makeBinPath [ nix nix-eval-jobs ]}
  '';

  meta = {
    description = "Deploy systemd portable services from Nix flakes, evaluated at runtime";
    license = lib.licenses.mit;
    mainProgram = "flakelet";
    platforms = lib.platforms.linux;
  };
}
