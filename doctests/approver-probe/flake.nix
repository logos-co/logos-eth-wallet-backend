{
  description = "approver_probe — doc-test fixture: the module that approves a signature.";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";
    # Pinned to the manager's first branch until it reaches main.
    signer_manager_module = {
      url = "github:logos-co/logos-signer-manager-module/67859da29c14e1ffe1da306e0c31fdafb33a0a02";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
    in
    {
      packages = nixpkgs.lib.genAttrs systems (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
