{
  description = "Logos eth_wallet_backend — Ethereum-only wallet coordinator (one active network, fixed token table, Send with full fee control).";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";
    # Every dependency must build against THIS module-builder. Without the follows each
    # drags its own, and a skewed generated ABI segfaults the module inside provider init.
    eth_rpc_module = {
      url = "github:logos-co/logos-evm-eth-rpc-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    fee_module = {
      url = "github:logos-co/logos-evm-fee-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    keystore_module = {
      url = "github:logos-co/logos-evm-keystore-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    # Metadata only — it decorates the allowlist and can never add a row to it. Declared
    # rather than reached untyped because it carries no external libraries of its own.
    token_list_module = {
      url = "github:logos-co/logos-evm-token-list-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
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
