{
  description = "Logos eth_wallet_backend — multi-chain EVM wallet composer over reusable modules.";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";
    # The follows is for LOCK SIZE: without it each dependency drags its own module-builder
    # subtree and this lock goes 753 -> 3741 nodes.
    #
    # It is NOT a compatibility measure, despite what this comment used to claim. Measured both
    # ways: the dependencies' published `.lidl` and the consumer code generated from it are
    # BYTE-IDENTICAL whether a dependency builds against its own module-builder or this one, and
    # both trees build. There is no ABI skew here to protect against.
    eth_rpc_module = {
      url = "github:logos-co/logos-evm-eth-rpc-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    fee_module = {
      url = "github:logos-co/logos-evm-fee-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
      inputs.eth_rpc_module.follows = "eth_rpc_module";
    };
    keystore_module = {
      url = "github:logos-co/logos-evm-keystore-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    # Catalogue and persisted enabled-token snapshots. The composer relays its explicit
    # enable/disable mutation while asset composition reads the same instance below.
    token_list_module = {
      url = "github:logos-co/logos-evm-token-list-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    # Reusable asset composition: native + enabled ERC-20 rows, balances, unsigned
    # transfers and history decoration. Its diamonds follow the same provider instances.
    evm_assets_module = {
      url = "github:logos-co/logos-evm-assets-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
      inputs.eth_rpc_module.follows = "eth_rpc_module";
      inputs.token_list_module.follows = "token_list_module";
    };
    # The one sender on the device. Every transaction this wallet makes leaves through it:
    # it reserves the nonce, asks the keystore, broadcasts and records.
    tx_sender_module = {
      url = "github:logos-co/logos-evm-tx-sender-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
      inputs.eth_rpc_module.follows = "eth_rpc_module";
      inputs.fee_module.follows = "fee_module";
      inputs.keystore_module.follows = "keystore_module";
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;

      # x86_64-windows is a cross PSEUDO-SYSTEM the builder already understands
      # (logos-module-builder lib/common.nix routes it to
      # logos-nix.lib.mkWindowsPkgs, and picks the build platform separately).
      # It is a target, never a host we evaluate nixpkgs natively for, so it
      # only ever belongs in `packages`.
      targets = systems ++ [ "x86_64-windows" ];
      forAllTargets = f: nixpkgs.lib.genAttrs targets f;
    in
    {
      packages = forAllTargets (system:
        (logos-module-builder.lib.mkLogosModule {
          src = ./.;
          configFile = ./metadata.json;
          flakeInputs = inputs;
        }).packages.${system});
    };
}
