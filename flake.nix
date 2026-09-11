{
	description = "A flake for building this Rust application";

	inputs = {
		nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
		flake-utils.url = "github:numtide/flake-utils";
	};

	outputs = {
		self,
		nixpkgs,
		flake-utils,
	}:
		flake-utils.lib.eachDefaultSystem (
			system: let
				pkgs = nixpkgs.legacyPackages.${system};

				cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);

				app =
					pkgs.rustPlatform.buildRustPackage {
						pname = cargoToml.package.name;
						version = cargoToml.package.version;
						src = ./.;

						cargoLock.lockFile = ./Cargo.lock;
					};
			in {
				# Enables `nix build`
				packages.default = app;

				# Enables `nix run`
				apps.default =
					flake-utils.lib.mkApp {
						drv = app;
					};
			}
		);
}
