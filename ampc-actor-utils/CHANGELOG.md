# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/worldcoin/ampc-common/releases/tag/ampc-actor-utils-v0.1.0) - 2026-10-30

### Added

- *(ampc-actor-utils)* allow for configurable mpc network handle timeout duration ([#141](https://github.com/worldcoin/ampc-common/pull/141))
- 2D NHD anon stats ([#75](https://github.com/worldcoin/ampc-common/pull/75))
- *(network)* improve serialization of vec ring ([#44](https://github.com/worldcoin/ampc-common/pull/44))
- *(rng)* use chacha8 instead of AesRng ([#43](https://github.com/worldcoin/ampc-common/pull/43))

### Fixed

- *(misc)* various memory improvements ([#45](https://github.com/worldcoin/ampc-common/pull/45))

### Other

- [POP-4367] Share startup gallery consistency checks ([#168](https://github.com/worldcoin/ampc-common/pull/168))
- [POP-4409] Share request validity synchronization ([#166](https://github.com/worldcoin/ampc-common/pull/166))
- Vectorize bit packing and remove the payload clone from ring sends ([#140](https://github.com/worldcoin/ampc-common/pull/140))
- Optimize exact-scan MPC threshold primitives ([#138](https://github.com/worldcoin/ampc-common/pull/138))
- [POP-4078] harden MPC handshake ([#122](https://github.com/worldcoin/ampc-common/pull/122))
- Add documentation to the networking stack ([#119](https://github.com/worldcoin/ampc-common/pull/119))
- Add a dedicated channel for control-plane messages in the networking stack ([#103](https://github.com/worldcoin/ampc-common/pull/103))
- [POP-3787] expose raw NetworkConnections in the networking layer ([#101](https://github.com/worldcoin/ampc-common/pull/101))
- [POP-3743] refactor networking connection logic for the worker pool ([#97](https://github.com/worldcoin/ampc-common/pull/97))
- [POP-2877] get rid of HashMap in TcpSession ([#99](https://github.com/worldcoin/ampc-common/pull/99))
- decouple cancellation token for sync_peers and network_sessions ([#95](https://github.com/worldcoin/ampc-common/pull/95))
- [POP-3693] use mTLS ([#92](https://github.com/worldcoin/ampc-common/pull/92))
- More tracing instrumentation to collect metrics ([#91](https://github.com/worldcoin/ampc-common/pull/91))
- [POP-3590] implement worker pool ([#84](https://github.com/worldcoin/ampc-common/pull/84))
- Remove nhd_comparison_nmr ([#89](https://github.com/worldcoin/ampc-common/pull/89))
- Make StateChecksum public ([#86](https://github.com/worldcoin/ampc-common/pull/86))
- Refactor conditional select and swap protocols ([#83](https://github.com/worldcoin/ampc-common/pull/83))
- Update Rust to 1.94.0 ([#85](https://github.com/worldcoin/ampc-common/pull/85))
- [POP-3591] create new module for worker pool networking ([#81](https://github.com/worldcoin/ampc-common/pull/81))
- Refactor `cross_mul` and use `bail` ([#82](https://github.com/worldcoin/ampc-common/pull/82))
- Move distance operations for FHD and NHD ([#80](https://github.com/worldcoin/ampc-common/pull/80))
- Bit packing before sending over network ([#77](https://github.com/worldcoin/ampc-common/pull/77))
- 1D anon stats for NHD ([#74](https://github.com/worldcoin/ampc-common/pull/74))
- [POP-3293] introduce new sync_on_job_hash for 32byte hashes ([#73](https://github.com/worldcoin/ampc-common/pull/73))
- NHD-specific operations for HNSW ([#72](https://github.com/worldcoin/ampc-common/pull/72))
- Generify shuffle protocol ([#66](https://github.com/worldcoin/ampc-common/pull/66))
- Generic conditional select ([#65](https://github.com/worldcoin/ampc-common/pull/65))
- Lifting to 48 bits ([#63](https://github.com/worldcoin/ampc-common/pull/63))
- add clippy lint forbidding iterating over hashmap/sets ([#64](https://github.com/worldcoin/ampc-common/pull/64))
- Support for 48-bit words ([#61](https://github.com/worldcoin/ampc-common/pull/61))
- Revert "add shutdown to sync ([#57](https://github.com/worldcoin/ampc-common/pull/57))" ([#58](https://github.com/worldcoin/ampc-common/pull/58))
- Revert "[POP-3443] Optimize open_bin by packing bits" ([#59](https://github.com/worldcoin/ampc-common/pull/59))
- [POP-3443] Optimize open_bin by packing bits ([#56](https://github.com/worldcoin/ampc-common/pull/56))
- add shutdown to sync ([#57](https://github.com/worldcoin/ampc-common/pull/57))
- [POP-3432] Distribute MSB extraction throughput more evenly ([#54](https://github.com/worldcoin/ampc-common/pull/54))
- bump metrics to 0.24 ([#53](https://github.com/worldcoin/ampc-common/pull/53))
- Add missing CI steps and fix linting ([#51](https://github.com/worldcoin/ampc-common/pull/51))
- faster bit inject ([#48](https://github.com/worldcoin/ampc-common/pull/48))
- use rng::fill() over rng::gen() ([#46](https://github.com/worldcoin/ampc-common/pull/46))
- [POP-3374] remove yield from the networking stack ([#41](https://github.com/worldcoin/ampc-common/pull/41))
- [POP-3212] Optimized A2B protocol ([#35](https://github.com/worldcoin/ampc-common/pull/35))
- Extend visibility of oblivious shuffle logic ([#34](https://github.com/worldcoin/ampc-common/pull/34))
- [POP-3211] Optimized bit injection ([#30](https://github.com/worldcoin/ampc-common/pull/30))
- Simplify Anon Stats server ([#20](https://github.com/worldcoin/ampc-common/pull/20))
- [POP-3114] Add ripple_carry_adder feature ([#9](https://github.com/worldcoin/ampc-common/pull/9))
- Implement gte_zero_and_open_u16 ([#10](https://github.com/worldcoin/ampc-common/pull/10))
- Add latest shuffle changes ([#15](https://github.com/worldcoin/ampc-common/pull/15))
- Address latest protocol changes ([#14](https://github.com/worldcoin/ampc-common/pull/14))
- Ampc anon stats [POP-3219] ([#11](https://github.com/worldcoin/ampc-common/pull/11))
- Prepare iris-mpc reuse ([#12](https://github.com/worldcoin/ampc-common/pull/12))
- Add custom deserializer to networking config ([#8](https://github.com/worldcoin/ampc-common/pull/8))
- Migrate networking, secret sharing and protocol utils from iris-mpc ([#7](https://github.com/worldcoin/ampc-common/pull/7))
