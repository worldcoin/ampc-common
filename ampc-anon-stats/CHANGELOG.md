# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/worldcoin/ampc-common/releases/tag/ampc-anon-stats-v0.1.0) - 2026-10-30

### Added

- *(POP-4266)* introduce anon stats sampling_rate for di 2d stats ([#143](https://github.com/worldcoin/ampc-common/pull/143))
- *(anon-stats)* add created_at + concurrent index to anon_stats_2d_di (POP-4104) ([#128](https://github.com/worldcoin/ampc-common/pull/128))
- add separate anon stats config for mirror ([#94](https://github.com/worldcoin/ampc-common/pull/94))
- 2D NHD anon stats ([#75](https://github.com/worldcoin/ampc-common/pull/75))
- prepare calculation of anon stats for reauth over a larger range ([#36](https://github.com/worldcoin/ampc-common/pull/36))

### Fixed

- propagate the correct parameters into BucketStatistics in Reauth ([#50](https://github.com/worldcoin/ampc-common/pull/50))

### Other

- Share anon-stats shutdown lifecycle across iris, DI and face ([#167](https://github.com/worldcoin/ampc-common/pull/167))
- Ignore unknown migrations config for anon stats ([#147](https://github.com/worldcoin/ampc-common/pull/147))
- introduce composite index for 2d di stats ([#146](https://github.com/worldcoin/ampc-common/pull/146))
- Add a flag to ignore unknown migrations ([#145](https://github.com/worldcoin/ampc-common/pull/145))
- add mpc_timeout in anon stats server config ([#144](https://github.com/worldcoin/ampc-common/pull/144))
- Optimize exact-scan MPC threshold primitives ([#138](https://github.com/worldcoin/ampc-common/pull/138))
- Add 2d anon stats opposite mirror batch config ([#136](https://github.com/worldcoin/ampc-common/pull/136))
- pop 4160 adjust deep identifier anon stats queries ([#133](https://github.com/worldcoin/ampc-common/pull/133))
- pop 4160 anon stats for mirror in di exclude anon stats for normal if ([#131](https://github.com/worldcoin/ampc-common/pull/131))
- [POP-4111] Add face anon stats operation support ([#127](https://github.com/worldcoin/ampc-common/pull/127))
- [POP-3878] Tfhard/anon stats di ([#120](https://github.com/worldcoin/ampc-common/pull/120))
- Decouple NHD anon-stats bucket count from FHD ([#124](https://github.com/worldcoin/ampc-common/pull/124))
- add created_at + index to anon_stats_* (for guarded-DELETE retention) ([#121](https://github.com/worldcoin/ampc-common/pull/121))
- [POP-3849] Added iris variant for deep identifier ([#116](https://github.com/worldcoin/ampc-common/pull/116))
- Decouple NHD anon-stats bucket threshold from FHD (POP-3904) ([#107](https://github.com/worldcoin/ampc-common/pull/107))
- Fix anon stats floating point precision on buckets ([#88](https://github.com/worldcoin/ampc-common/pull/88))
- [POP-3591] create new module for worker pool networking ([#81](https://github.com/worldcoin/ampc-common/pull/81))
- Refactor `cross_mul` and use `bail` ([#82](https://github.com/worldcoin/ampc-common/pull/82))
- Move distance operations for FHD and NHD ([#80](https://github.com/worldcoin/ampc-common/pull/80))
- make AnonStatsMapping clone ([#78](https://github.com/worldcoin/ampc-common/pull/78))
- add process_2d_lifted_anon_stats_job for HNSW 2D support ([#70](https://github.com/worldcoin/ampc-common/pull/70))
- 1D anon stats for NHD ([#74](https://github.com/worldcoin/ampc-common/pull/74))
- Add AnonStatsOperation::Recovery variant for distinct recovery stats ([#69](https://github.com/worldcoin/ampc-common/pull/69))
- Feat/process 2d mirror ([#68](https://github.com/worldcoin/ampc-common/pull/68))
- parse anon stats mirror ([#67](https://github.com/worldcoin/ampc-common/pull/67))
- add clippy lint forbidding iterating over hashmap/sets ([#64](https://github.com/worldcoin/ampc-common/pull/64))
- populate stat time utc correctly ([#52](https://github.com/worldcoin/ampc-common/pull/52))
- use rng::fill() over rng::gen() ([#46](https://github.com/worldcoin/ampc-common/pull/46))
- validate config values ([#38](https://github.com/worldcoin/ampc-common/pull/38))
- cap max rows fetched per job ([#37](https://github.com/worldcoin/ampc-common/pull/37))
- [POP-3212] Optimized A2B protocol ([#35](https://github.com/worldcoin/ampc-common/pull/35))
- [POP-3317] Granular configuration for face AMPC ([#32](https://github.com/worldcoin/ampc-common/pull/32))
- anon stats config for tls ([#33](https://github.com/worldcoin/ampc-common/pull/33))
- Set end_time_utc_timestamp in face anon stats ([#31](https://github.com/worldcoin/ampc-common/pull/31))
- [POP-3211] Optimized bit injection ([#30](https://github.com/worldcoin/ampc-common/pull/30))
- [POP-3208] feat: implement Anon stats for face-ampc ([#21](https://github.com/worldcoin/ampc-common/pull/21))
- add constructor params ([#29](https://github.com/worldcoin/ampc-common/pull/29))
- anon stats table requirements and its types [POP-3254] ([#27](https://github.com/worldcoin/ampc-common/pull/27))
- (patch) fix network cancellation token ([#28](https://github.com/worldcoin/ampc-common/pull/28))
- separate configs for 1d and 2d stats ([#24](https://github.com/worldcoin/ampc-common/pull/24))
- add source field ([#23](https://github.com/worldcoin/ampc-common/pull/23))
- sns bucket ([#22](https://github.com/worldcoin/ampc-common/pull/22))
- Simplify Anon Stats server ([#20](https://github.com/worldcoin/ampc-common/pull/20))
- add batch sync utilities ([#19](https://github.com/worldcoin/ampc-common/pull/19))
- common coordinator ([#16](https://github.com/worldcoin/ampc-common/pull/16))
- Address latest protocol changes ([#14](https://github.com/worldcoin/ampc-common/pull/14))
- Ampc anon stats [POP-3219] ([#11](https://github.com/worldcoin/ampc-common/pull/11))
