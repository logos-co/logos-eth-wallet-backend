{
  description = "Logos eth_wallet_backend — Ethereum-only wallet coordinator (one active network, fixed token table, Send with full fee control).";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";
    eth_rpc_module.url = "github:logos-co/logos-evm-eth-rpc-module";
    fee_module.url = "github:logos-co/logos-evm-fee-module";
    keystore_module.url = "github:logos-co/logos-evm-keystore-module";
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;
    in
    {
      packages = forAllSystems (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
