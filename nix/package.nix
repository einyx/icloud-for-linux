{ lib
, rustPlatform
, pkg-config
, installShellFiles
, makeWrapper
}:

rustPlatform.buildRustPackage rec {
  pname = "icloud-for-linux";
  version = "0.6.0";

  # The build directory and the git history are not part of the source.
  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: _type:
      let base = baseNameOf (toString path);
      in !(base == "target" || base == "dist" || base == ".git");
  };
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config installShellFiles makeWrapper ];

  postInstall = ''
    install -Dm644 docs/icloudctl.1 $out/share/man/man1/icloudctl.1
    install -Dm644 config.example.yaml \
      $out/etc/icloud-for-linux/config.yaml.example
    install -Dm644 packaging/systemd/icloud-for-linux.service \
      $out/share/systemd/user/icloud-for-linux.service
  '';

  # The tests need a FUSE device, which is not available in the sandbox.
  doCheck = false;

  meta = with lib; {
    description = "Mount iCloud Drive on Linux as a local-first FUSE filesystem";
    homepage = "https://github.com/einyx/icloud-for-linux";
    license = licenses.gpl3Only;
    platforms = platforms.linux;
    mainProgram = "icloudctl";
  };
}
