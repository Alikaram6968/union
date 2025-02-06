{ inputs, ... }:
{
  perSystem =
    {
      self',
      crane,
      pkgs,
      dbg,
      ...
    }:
    let
      bytecode-base = pkgs.stdenv.mkDerivation {
        name = "base-bytecode";
        dontUnpack = true;
        src = ../tools/cosmwasm-deployer/base-bytecode.wat;
        buildInputs = [ pkgs.binaryen ];
        buildPhase = ''
          wasm-as $src -o $out
        '';
      };

      cosmwasm-deployer = crane.buildWorkspaceMember {
        crateDirFromRoot = "cosmwasm/deployer";
      };

      deploy-full =
        args@{
          name,
          rpc_url,
          gas_config,
          private_key,
          ...
        }:
        pkgs.writeShellApplication {
          name = "${name}-deploy-full";
          runtimeInputs = [ cosmwasm-deployer.packages.cosmwasm-deployer ];
          text = ''
            RUST_LOG=info \
              cosmwasm-deployer \
              deploy-full \
              --private-key ${private_key} \
              --gas-price ${toString gas_config.gas_price} \
              --gas-denom ${toString gas_config.gas_denom} \
              --gas-multiplier ${toString gas_config.gas_multiplier} \
              --max-gas ${toString gas_config.max_gas} \
              --contracts ${mk-chain-deployments-json args} \
              --rpc-url ${rpc_url} "$@"
          '';
        };

      # directory => {}
      all-lightclients = {
        arbitrum = { };
        berachain = { };
        cometbls = { };
        ethereum = { };
        # ethermint = {};
        tendermint = { };
        movement = { };
        state-lens-ics23-mpt = { };
      };

      ucs03-configs = {
        cw20 = {
          path = "${self'.packages.ucs03-zkgm}";
          token_minter_path = "${self'.packages.cw20-token-minter}";
          token_minter_config = {
            cw20 = {
              cw20_base = "${pkgs.fetchurl {
                url = "https://github.com/CosmWasm/cw-plus/releases/download/v2.0.0/cw20_base.wasm";
                sha256 = "sha256-TY6Q3TQJkwM/G56OOj7n+Gc8WCypvN2MjPPHRw1lN9U=";
              }}";
            };
          };
        };
        native = {
          path = "${self'.packages.ucs03-zkgm}";
          token_minter_path = "${self'.packages.token-factory-minter}";
          token_minter_config = {
            native = { };
          };
        };
      };

      # client type => package name
      all-apps = {
        ucs00-pingpong = {
          name = "ucs00-pingpong";
        };
        ucs03-zkgm = {
          name = "ucs03-zkgm";
        };
      };

      mk-chain-deployments-json =
        { lightclients, apps, ... }:
        pkgs.writeText "contracts.json" (
          builtins.toJSON {
            core = ibc-union;
            lightclient = pkgs.lib.mapAttrs (n: _: mk-lightclient n) (
              pkgs.lib.filterAttrs (n: _: builtins.elem n lightclients) all-lightclients
            );
            app = apps;
          }
        );

      mk-cosmwasm-ibc-union-addresses =
        {
          name,
          bech32_prefix,
          private_key,
          lightclients,
          apps,
          ...
        }:
        pkgs.stdenv.mkDerivation {
          name = "${name}-cosmwasm-ibc-union-addresses";
          dontUnpack = true;
          src = [ ];
          buildInputs = [ cosmwasm-deployer.packages.cosmwasm-deployer ];
          buildPhase = ''
            cosmwasm-deployer \
              addresses \
              --bech32-prefix ${bech32_prefix} \
              ${pkgs.lib.strings.concatStrings (map (l: " --lightclient ${l}") lightclients)} \
              ${pkgs.lib.strings.concatStrings (map (l: " --${l}") (builtins.attrNames apps))} \
              --private-key ${private_key} \
              --output $out
          '';
        };

      networks = [
        {
          name = "union-devnet";
          rpc_url = "http://localhost:26657";
          # alice from the devnet keyring
          private_key = "0xaa820fa947beb242032a41b6dc9a8b9c37d8f5fbcda0966b1ec80335b10a7d6f";
          gas_config = {
            gas_denom = "muno";
            gas_multiplier = "1.1";
            gas_price = "1.0";
            max_gas = 10000000;
          };
          ucs03_type = "cw20";
          bech32_prefix = "union";
          apps = {
            ucs03 = ucs03-configs.cw20;
          };
          lightclients = pkgs.lib.lists.remove "cometbls" (builtins.attrNames all-lightclients);
        }
        {
          name = "union-testnet";
          rpc_url = "https://rpc.testnet-9.union.build";
          private_key = "$1";
          gas_config = {
            gas_denom = "muno";
            gas_multiplier = "1.1";
            gas_price = "1.0";
            max_gas = 10000000;
          };
          apps = {
            ucs03 = ucs03-configs.cw20;
          };
          bech32_prefix = "union";
          lightclients = pkgs.lib.lists.remove "cometbls" (builtins.attrNames all-lightclients);
        }
      ];

      mk-lightclient =
        dir:
        (crane.buildWasmContract {
          crateDirFromRoot = "cosmwasm/ibc-union/lightclient/${dir}";
        });

      mk-app =
        dir:
        (crane.buildWasmContract {
          crateDirFromRoot = "cosmwasm/ibc-union/app/${dir}";
        });

      # ucs00-pingpong = crane.buildWasmContract {
      #   crateDirFromRoot = "cosmwasm/ucs00-pingpong";
      # };

      cw721-base = crane.buildRemoteWasmContract {
        src = inputs.cosmwasm-nfts;
        version = inputs.cosmwasm-nfts.rev;
        package = "cw721-base@0.18.0";
        contractFileNameWithoutExt = "cw721_base";
      };

      ucs03-zkgm = crane.buildWasmContract {
        crateDirFromRoot = "cosmwasm/ibc-union/app/ucs03-zkgm";
      };

      ibc-union = crane.buildWasmContract {
        crateDirFromRoot = "cosmwasm/ibc-union/core";
      };

      multicall = crane.buildWasmContract {
        crateDirFromRoot = "cosmwasm/multicall";
      };

      native-token-minter = crane.buildWasmContract {
        crateDirFromRoot = "cosmwasm/native-token-minter";
      };

      cw20-token-minter = crane.buildWasmContract {
        crateDirFromRoot = "cosmwasm/cw20-token-minter";
      };
    in
    {
      packages =
        {
          inherit
            bytecode-base
            cw721-base
            ucs03-zkgm
            native-token-minter
            cw20-token-minter
            ibc-union
            multicall
            ;
          # all-deployments-json = mk-all-deployments-json;cosmwasm-ibc-union-addresses;
          cosmwasm-scripts =
            (
              (builtins.listToAttrs (
                map (args: {
                  name = "${args.name}-ibc-union-contract-addresses";
                  value = mk-cosmwasm-ibc-union-addresses args;
                }) networks
              ))
              // (builtins.listToAttrs (
                map (args: {
                  name = "${args.name}-chain-deployments-json";
                  value = mk-chain-deployments-json args;
                }) networks
              ))
              // (builtins.listToAttrs (
                map (args: {
                  name = "${args.name}-deploy-full";
                  value = deploy-full args;
                }) networks
              ))
            )
            // derivation { };
        }
        // cosmwasm-deployer.packages
        //
          # all light clients
          (pkgs.lib.mapAttrs' (n: _v: rec {
            name = dbg value.passthru.packageName;
            value = mk-lightclient n;
          }) all-lightclients)
        //
          # all apps
          (builtins.foldl' (x: y: x // y) { } (map mk-app (builtins.attrNames all-apps)));
    };
}
