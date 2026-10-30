# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/worldcoin/ampc-common/releases/tag/ampc-secret-sharing-v0.1.0) - 2026-10-30

### Fixed

- *(misc)* various memory improvements ([#45](https://github.com/worldcoin/ampc-common/pull/45))

### Other

- Decrypt binary shares ([#117](https://github.com/worldcoin/ampc-common/pull/117))
- [POP-3999] feat: introduce base64 encode/decode functions for IrisVector and shared ([#112](https://github.com/worldcoin/ampc-common/pull/112))
- Tfhard/iris vector, random shares generation bug fix  ([#109](https://github.com/worldcoin/ampc-common/pull/109))
- [POP-3848] Define `IrisVector` and `IrisSecretSharedVector` types. ([#108](https://github.com/worldcoin/ampc-common/pull/108))
- Refactor `cross_mul` and use `bail` ([#82](https://github.com/worldcoin/ampc-common/pull/82))
- 1D anon stats for NHD ([#74](https://github.com/worldcoin/ampc-common/pull/74))
- Generify shuffle protocol ([#66](https://github.com/worldcoin/ampc-common/pull/66))
- Lifting to 48 bits ([#63](https://github.com/worldcoin/ampc-common/pull/63))
- add clippy lint forbidding iterating over hashmap/sets ([#64](https://github.com/worldcoin/ampc-common/pull/64))
- Support for 48-bit words ([#61](https://github.com/worldcoin/ampc-common/pull/61))
- Add missing CI steps and fix linting ([#51](https://github.com/worldcoin/ampc-common/pull/51))
- use rng::fill() over rng::gen() ([#46](https://github.com/worldcoin/ampc-common/pull/46))
- [POP-3212] Optimized A2B protocol ([#35](https://github.com/worldcoin/ampc-common/pull/35))
- [POP-3211] Optimized bit injection ([#30](https://github.com/worldcoin/ampc-common/pull/30))
- [POP-3208] feat: implement Anon stats for face-ampc ([#21](https://github.com/worldcoin/ampc-common/pull/21))
- Add raw and secret shared vector structs ([#18](https://github.com/worldcoin/ampc-common/pull/18))
- Address latest protocol changes ([#14](https://github.com/worldcoin/ampc-common/pull/14))
- Ampc anon stats [POP-3219] ([#11](https://github.com/worldcoin/ampc-common/pull/11))
- Add missing deg_1_lagrange_polys_at_zero function to galois module ([#13](https://github.com/worldcoin/ampc-common/pull/13))
- Prepare iris-mpc reuse ([#12](https://github.com/worldcoin/ampc-common/pull/12))
- Migrate networking, secret sharing and protocol utils from iris-mpc ([#7](https://github.com/worldcoin/ampc-common/pull/7))
