# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/worldcoin/ampc-common/releases/tag/ampc-server-utils-v0.1.0) - 2026-10-30

### Added

- *(server-coordination)* startup-barrier duration + outcome metrics (POP-4162) ([#134](https://github.com/worldcoin/ampc-common/pull/134))
- *(server-coordination)* wait_for_startup_barriers — combined unready + visibility startup barrier (POP-4162) ([#132](https://github.com/worldcoin/ampc-common/pull/132))

### Fixed

- *(server-coordination)* retry startup unready-check in-process — kills the rolling-restart fleet wedge ([#130](https://github.com/worldcoin/ampc-common/pull/130))

### Other

- [POP-4367] Share startup gallery consistency checks ([#168](https://github.com/worldcoin/ampc-common/pull/168))
- [POP-4441] Bound peer heartbeat requests through response-body reads ([#169](https://github.com/worldcoin/ampc-common/pull/169))
- [POP-4279] Require explicit modification input storage ([#164](https://github.com/worldcoin/ampc-common/pull/164))
- [POP-4279] Share S3 and inline modification inputs ([#163](https://github.com/worldcoin/ampc-common/pull/163))
- [POP-4279] Share Iris modification storage and recovery lifecycle ([#162](https://github.com/worldcoin/ampc-common/pull/162))
- [POP-4279] Extract Iris modification reconciliation ([#161](https://github.com/worldcoin/ampc-common/pull/161))
- pop 4347 replace sodiumoxide with alkali in ampc common ([#155](https://github.com/worldcoin/ampc-common/pull/155))
- harden the heartbeat task and wait_for_others_ready ([#129](https://github.com/worldcoin/ampc-common/pull/129))
- Demote noisy coordination-server logs to debug ([#118](https://github.com/worldcoin/ampc-common/pull/118))
- Decrypt binary shares ([#117](https://github.com/worldcoin/ampc-common/pull/117))
- Harden batch sync polling against transient failures ([#113](https://github.com/worldcoin/ampc-common/pull/113))
- migrate startup sync function from iris-mpc ([#106](https://github.com/worldcoin/ampc-common/pull/106))
- Add coordination server logging and startup-sync debug visibility ([#105](https://github.com/worldcoin/ampc-common/pull/105))
- Add HTTP query timeout config and apply in server coordination ([#104](https://github.com/worldcoin/ampc-common/pull/104))
- Revert "add config endpoint ([#79](https://github.com/worldcoin/ampc-common/pull/79))" ([#90](https://github.com/worldcoin/ampc-common/pull/90))
- [POP-3590] implement worker pool ([#84](https://github.com/worldcoin/ampc-common/pull/84))
- add config endpoint ([#79](https://github.com/worldcoin/ampc-common/pull/79))
- Refactor `cross_mul` and use `bail` ([#82](https://github.com/worldcoin/ampc-common/pull/82))
- [POP-3391] treat timeouts as errors ([#42](https://github.com/worldcoin/ampc-common/pull/42))
- support additional route ([#71](https://github.com/worldcoin/ampc-common/pull/71))
- add clippy lint forbidding iterating over hashmap/sets ([#64](https://github.com/worldcoin/ampc-common/pull/64))
- [POP-3208] feat: implement Anon stats for face-ampc ([#21](https://github.com/worldcoin/ampc-common/pull/21))
- add constructor params ([#29](https://github.com/worldcoin/ampc-common/pull/29))
- (patch) fix network cancellation token ([#28](https://github.com/worldcoin/ampc-common/pull/28))
- add second cancellation token just for the networking stack ([#26](https://github.com/worldcoin/ampc-common/pull/26))
- [POP-3167]  Add generic CommonConfig trait in StartupSyncState ([#25](https://github.com/worldcoin/ampc-common/pull/25))
- add source field ([#23](https://github.com/worldcoin/ampc-common/pull/23))
- Simplify Anon Stats server ([#20](https://github.com/worldcoin/ampc-common/pull/20))
- add batch sync utilities ([#19](https://github.com/worldcoin/ampc-common/pull/19))
- common coordinator ([#16](https://github.com/worldcoin/ampc-common/pull/16))
- Address latest protocol changes ([#14](https://github.com/worldcoin/ampc-common/pull/14))
- Ampc anon stats [POP-3219] ([#11](https://github.com/worldcoin/ampc-common/pull/11))
- Add share decryption utils using Secrets Manager ([#6](https://github.com/worldcoin/ampc-common/pull/6))
- Add sqs sync utils ([#5](https://github.com/worldcoin/ampc-common/pull/5))
- Add server coordination utils ([#4](https://github.com/worldcoin/ampc-common/pull/4))
