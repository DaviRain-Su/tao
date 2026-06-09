# Tao 链 — PoW 公链实现规划

## Context（为什么做这件事）

目标：从头实现一条 **PoW 挖矿的公链**，核心诉求有四：

1. **兼容 Solana 生态**（而非以太坊）——让现有 SPL Token / Anchor 程序、Phantom 钱包、`@solana/web3.js` 能直接用。
2. **PoW 共识**，挖矿硬件按 CPU → GPU → 高端 GPU 演进。
3. 探索 **AI/ML 结合**——让挖矿算力本身就是 AI 计算（参考已主网上线的 **Pearl/PRL** 的 matmul-PoUW）。
4. 用 **Rust** 实现，参考 **Kaspa** 的快速 PoW 与 **Pearl** 的「AI 化 PoW」。

经过四个方向的调研 + 用户拍板，确定的战略路线：

- **语言：Rust**（压倒性选择）。决定性原因——Solana 的 SVM（`solana-svm`，Apache-2.0）本身就是 Rust 写的、已被 Anza 拆成可独立嵌入的 crate；rusty-kaspa（ISC）也是 Rust。Zig 生态（pre-1.0 动荡、crypto/网络需手搓）不适合小团队，且无法干净复用 SVM。
- **共识：分阶段**。阶段 1 做**线性 PoW 链 + SVM**（成熟路径，类似 Pythnet 换共识 / Eclipse 复用 SVM），先端到端跑通；阶段 2 再把共识研究升级为 **blockDAG/GHOSTDAG**。两条线解耦推进，风险最低。
- **兼容深度：完整 SVM 智能合约**（Tier 1）。
- **PoW 演进：RandomX（CPU 公平启动）→ matmul-PoUW（GPU，AI 化，ZK 验证）硬分叉**。GPU 阶段不用 kHeavyHash，改用 Pearl 式的「矩阵乘法当 PoW」，让算力天然是 AI 计算。保持消费级 GPU 友好（不锁 Hopper）。
- **AI 整合是核心而非边角**：matmul-PoUW 让每次 GPU 矩阵乘既挖矿又能复用于真实 AI 推理，并要求**真正有用**——把证明绑定到真实模型/付费推理任务（修复 Pearl「现网≈零真实 AI」的核心缺陷）。
- **定位：激进复用成熟 crate**，站在巨人肩上，尽快得到可用链。

### 关键架构判断（来自调研）

- **SVM 与共识解耦**：`solana-svm` 只做「加载账户 + 执行」，**不管**共识、排序、账户锁、收费、状态提交——这些是我们在上层「Bank/Runtime」层自己实现。所以 SVM 可以跑在任何共识下，**包括 PoW**。
- **线性 PoW + SVM = 干净可行（Case A）**：单链单块、最长链规则；块内用 Solana 交易自带的「声明账户读写集」做 Sealevel 式并行执行，块边界提交状态。执行语义与 Solana **完全一致**，程序无需改动。与 Solana 的差异仅是出块时间（秒级 vs 400ms），不影响兼容性。
- **blockDAG + SVM = 前沿研究（Case B，阶段 2）**：DAG 多块并行没有先验全序，必须先做「确定性线性化」再喂给 SVM；热点账户（如 token mint）冲突会吃掉大部分并行度。把它**与 SVM 兼容解耦**，独立 de-risk。
- **AI 整合走 matmul-PoUW，不走「训练当谜题」**：用 ML *训练* 当 PoW 谜题**不成立**（非确定性、验证不便宜，proof-of-learning 已被证明可低成本伪造）。Pearl 找到的正确路子是把**确定性的矩阵乘法**当谜题：给输入加低秩噪声 → 对全部中间块乘积的转录做哈希比对难度 → 命中后用 **Plonky2 STARK** 生成 ~60KB 证明，节点**毫秒级验证、无需重算**（论文 arXiv 2504.09971，理论开销仅 `1+o(1)`）。矩阵乘正是 AI 的核心算子，因此同一次计算可复用于真实推理。
- **「真正有用」是我们要补的硬骨头**：独立研究（arXiv 2606.04819）指出 Pearl 现网用随机矩阵、做的是「**AI 形状的 PoW 而非有用功**」——协议只验证「算得对」不验证「有用」，且有用性检查可零成本伪造。我们的目标是**把证明绑定到真实模型/付费推理任务**（utility gate），这是 Pearl 没解决的开放问题，单列研究轨。
- **ML 防御层仍保留（互补、共识链路之外）**：P2P 异常/eclipse 检测、51% 算力突增预警、mempool 反垃圾，纯 advisory，**绝不**参与区块有效性判定。
- **matmul-PoUW 的代价（诚实标注）**：依赖一个**未充分检验的「直积」矩阵转录硬度猜想**（不如 SHA-256/RandomX 久经考验）；需自建/移植 Plonky2 STARK 证明设施；复杂度与攻击面显著高于 kHeavyHash。因此排在 MVP（线性 PoW + SVM + RandomX）跑通**之后**。

---

## 架构总览（阶段 1：线性 PoW + SVM）

```
              ┌─────────────────────────────────────────┐
              │            tao-node (daemon)             │
              └─────────────────────────────────────────┘
   ┌──────────┬──────────────┬─────────────┬─────────────┐
   │  共识层   │   执行层      │   网络层     │   接口层     │
   │ Consensus│   Runtime     │    P2P      │    RPC      │
   ├──────────┼──────────────┼─────────────┼─────────────┤
   │ 区块/头   │ Bank(收费/    │ libp2p gossip│ JSON-RPC    │
   │ 最长链选择 │  rent/sysvar) │ 区块/交易中继 │ (Solana兼容) │
   │ RandomX  │ AccountsDB    │ IBD 同步     │ pubsub      │
   │ LWMA难度  │ solana-svm   │ Mempool     │ Faucet      │
   │ RocksDB链 │ 执行+状态提交  │             │             │
   └──────────┴──────────────┴─────────────┴─────────────┘
```

**交易生命周期**：钱包(web3.js) → `sendTransaction` RPC → Mempool → 矿工组块 → RandomX 挖矿 → 块内按声明账户锁调度执行(solana-svm) → 提交账户状态 + 写入 state_root → gossip 广播 → 其他节点重放校验。

---

## Crate 复用清单（激进复用）

| 用途 | Crate | License | 说明 |
|---|---|---|---|
| SVM 执行核心 | `solana-svm` (`TransactionBatchProcessor`) | Apache-2.0 | 我们的核心依赖；实现 `TransactionProcessingCallback` 对接账户存储 |
| 指令运行时 | `solana-program-runtime` | Apache-2.0 | invoke context、compute budget |
| sBPF 虚拟机 | `solana-sbpf`(原 `solana-rbpf`) | Apache-2.0 | JIT/解释器 |
| 核心类型 | `solana-sdk` 拆分包(`solana-account`/`-pubkey`/`-transaction`/`-message`/`-instruction`/`-hash`) | Apache-2.0 | 账户、ed25519、base58、交易格式 |
| 内置程序 | `solana-system-program` + BPF Loader 内置 | Apache-2.0 | 必需 builtins |
| 代币标准 | `spl-token` / `spl-associated-token-account` / Token-2022 | Apache-2.0 | 直接部署同款 sBPF 二进制 |
| SVM 测试夹具 | `mollusk-svm` | Apache-2.0 | 轻量执行/测试 harness |
| PoW(CPU) | `randomx-rs`(tevador RandomX 的 Rust 绑定) | — | 阶段1 RandomX |
| PoW(GPU) matmul | Pearl `pearl-gemm`(CUDA NoisyGEMM 核, 基于 CUTLASS) | ISC | 阶段2 matmul-PoUW 的噪声矩阵乘 + PoW 提取；移植以支持消费级 GPU(SM≥8.x)，不锁 Hopper |
| PoW(GPU) ZK 证明 | Pearl `zk-pow`(Rust, Plonky2 + STARKy + blake3) | ISC | 矿工证明电路 + 节点验证器；STARK 无可信设置、抗量子、~60KB 证明 |
| ZK 底层 | `plonky2` / `starky`(Polygon Zero) | — | Pearl `zk-pow` 的依赖，FRI-based STARK |
| 存储 | `rust-rocksdb` | — | 链 + 账户持久化（参考 rusty-kaspa `kaspa-database`） |
| 异步/网络 | `tokio` + `libp2p` | — | gossip、IBD |
| RPC | `jsonrpsee`(HTTP + WS) | — | Solana 兼容 RPC |
| 架构蓝本 | `rusty-kaspa`(~70 crate workspace, ISC) | ISC | 可读可改，作为 crate 边界/网络/pruning 设计参考 |

> 注：阶段 2 GPU 算法采用 matmul-PoUW（非 kHeavyHash）。Pearl 是 btcd/UTXO 的 Go 链，与我们 Solana 兼容的账户模型架构不同，因此**只抬取 PoW 相关组件**（`zk-pow`、`pearl-gemm`、可参考其 `difficulty.go` WTEMA），不复用其节点。matmul-PoUW 与执行层正交，可干净叠加在我们的链上。

---

## Workspace 结构（参考 rusty-kaspa / agave 边界）

```
tao/
├── Cargo.toml                  # workspace
├── crates/
│   ├── tao-node/               # 守护进程 binary，编排各 service
│   ├── tao-core/               # service 生命周期框架（参考 kaspa core/）
│   ├── tao-consensus/          # 区块/头类型、PoW 校验、最长链、难度调整
│   │   └── pow/                # PoW 算法抽象 trait + RandomX(阶段1) + matmul-PoUW(阶段2)
│   ├── tao-pouw/               # （阶段2）matmul-PoUW：NoisyGEMM 接口 + zk-pow 证明/验证 + utility gate
│   ├── tao-runtime/            # Bank 层：AccountsDB + solana-svm 集成 + sysvar/费用/rent
│   ├── tao-database/           # RocksDB 封装 + 缓存
│   ├── tao-p2p/                # libp2p 网络：区块/交易中继、IBD
│   ├── tao-mempool/            # 交易池 + 区块模板构建
│   ├── tao-rpc/                # JSON-RPC（Solana 兼容）+ pubsub
│   ├── tao-wallet/             # ed25519/BIP32 密钥、交易构造
│   └── tao-cli/                # CLI 钱包 + faucet + 运维
└── programs/                   # 预部署 sBPF 程序（SPL Token/ATA 二进制）
```

---

## 实现里程碑（按依赖顺序，MVP 优先）

### M0 — 脚手架
- 建 Cargo workspace + 上述 crate 骨架；接入 `solana-sdk` 拆分类型（直接复用 `Pubkey`/`Account`/`Transaction`/`Hash`/`Signature`，不自造）。
- 统一错误类型、日志(tracing)、配置加载、genesis 配置文件格式。
- **验证**：`cargo build` 通过；`tao-node --version` 可运行。

### M1 — PoW + 线性共识引擎（先不接执行）
- `Block`/`BlockHeader` 类型：`prev_hash, height, timestamp, nonce, difficulty_target, tx_merkle_root, state_root, miner_pubkey`（state_root 此阶段先留空/占位）。
- **PoW 算法抽象**：定义 `PowAlgorithm` trait（`verify(header, proof) -> bool`、`difficulty_from(...)`），让 RandomX 与后续 matmul-PoUW 可按激活高度切换。区块头预留 `pow_proof` 字段（RandomX 阶段为 nonce，matmul 阶段为 ~60KB STARK 证明的承诺/引用）。
- 集成 `randomx-rs`：区块头哈希 = RandomX(header_bytes)，与 target 比较。
- **难度调整 = 每块滑动窗口 LWMA**（Zawy-LWMA，窗口 ~60–120 块）；**禁用** Bitcoin 式 epoch 重定向。
- 最长链选择 = **累计工作量最大**（cumulative work）；含基础重组逻辑。
- 区块校验 + RocksDB 链存储（参考 `kaspa-database` 的列族/缓存策略）。
- 单节点矿工：能持续产出（空）区块、调整难度。
- **验证**：本地起单节点，观察稳定出块、难度随算力变化收敛；`cargo test` 覆盖难度算法与最长链选择。

### M2 — 账户状态层 + SVM 执行（关键里程碑）
- `AccountsDB`：RocksDB 支撑的账户存储。
- 实现 `TransactionProcessingCallback`（`get_account_shared_data` / `account_matches_owners` / `add_builtin_account`）——**这是 SVM 集成的核心对接点**。
- **Bank 层**：`BlockhashQueue`（最近 ~150 块哈希，供 `getLatestBlockhash` 与交易过期/去重）、费用收取（按签名数 + priority fee，路由到 coinbase）、rent（默认 rent-exempt）、sysvar 更新（`Clock`/`Rent`/`EpochSchedule` 等）。
- 嵌入 `solana-svm` 的 `TransactionBatchProcessor`，`load_and_execute_sanitized_transactions`。
- 部署内置程序：System program + BPF Loader(s)。
- 块内执行：MVP **顺序执行**即可（并行作为后续优化）；执行后提交账户状态、计算 `state_root`（账户增量哈希；先用简单确定性 accounts hash，后续可换 Sparse Merkle Tree）。
- coinbase 交易：矿工区块奖励（含初版 tokenomics：初始奖励 + 减半计划）。
- **验证**：构造一笔 System program 转账交易，矿工组块执行后，两个独立节点重放得到**一致的 state_root**；账户余额正确变化。

### M3 — SPL Token + 程序部署
- 支持用户通过 BPF Loader 部署 sBPF 程序。
- 预部署 SPL Token + ATA 程序（同款二进制）。
- **验证**：用 Solana CLI / web3.js 创建 mint、铸币、转账 SPL Token 成功；跑通一个最小 Anchor 程序（链侧零改动）。

### M4 — JSON-RPC 兼容层
- 实现核心 RPC 子集（钱包/web3.js 发送+确认交易实际调用的）：
  `getLatestBlockhash, sendTransaction, simulateTransaction, getSignatureStatuses, getAccountInfo, getMultipleAccounts, getBalance, getProgramAccounts, getTransaction, getMinimumBalanceForRentExemption, getSlot, getHealth`。
- pubsub：`signatureSubscribe, accountSubscribe`。
- **验证**：本地把 `@solana/web3.js` 的 `Connection` 指向我们节点，完成一次完整转账并确认；**Phantom 钱包**自定义 RPC 接入后能显示余额、发交易。

### M5 — P2P 网络 + 多节点
- libp2p gossip：区块中继(inv→getdata)、交易中继、孤块管理。
- IBD（初始区块下载）：headers-first 同步。
- Mempool 接入网络（交易广播）。
- **验证**：本地起 3 节点测试网，一个节点挖出的块/交易能传播到全网并被独立校验；新节点能从零 IBD 同步到链尾。

### M6 — 钱包 / CLI / Devnet
- `tao-wallet`：ed25519/BIP32 密钥管理、交易构造、签名。
- `tao-cli`：交互式钱包、faucet（测试币水龙头）、节点运维命令。
- genesis + 初始分配 + devnet 启动脚本。
- **验证**：从 faucet 领币 → CLI 钱包转账 → 区块浏览器/RPC 查询确认，端到端走通；发布可复现的 devnet 启动文档。

### M7 — GPU 阶段：matmul-PoUW（AI 化 PoW）核心研究与实现
分三步推进，每步可独立验证；这是项目最大的原创性与风险所在。

**M7a — matmul-PoUW 共识机制（先不绑定真实模型，先把 AI 形状的 PoW 跑通）**
- 移植/封装 Pearl `pearl-gemm` CUDA 核：NoisyGEMM（给 A、B 加低秩噪声 → 算噪声积 → 对 `(n/r)³` 中间块转录做 Blake3 哈希 → 比对难度 → 低秩修正恢复真积）。**移植到消费级 GPU（SM≥8.x，不锁 Hopper）**，保留 CPU 回退实现供测试。
- 集成 Pearl `zk-pow`（Plonky2 + STARKy）：矿工对「GEMM 正确执行且命中难度」生成 ~60KB STARK 证明；节点验证器毫秒级校验，无需重算。
- 接入 M1 的 `PowAlgorithm` trait + 难度抽象（WTEMA，参考 Pearl `difficulty.go`）。
- 矩阵规模/精度作为可调参数：大矩阵 + 低精度（INT8/FP8）天然偏好更强 GPU → 实现阶段 3「消费级 GPU → 高端 GPU」的平滑过渡（无需再硬分叉）。
- **算法切换硬分叉框架**：按预定高度从 RandomX 激活 matmul-PoUW；切换时**重置难度 + 加检查点 + 临时放宽 DAA 窗口**，抵御低难度窗口的租用算力攻击。RandomX 2024 已被 ASIC 攻破，CPU 阶段应短。
- **验证**：消费级 GPU（如 RTX 30/40 系）能跑 NoisyGEMM 挖矿；任意节点用 ~60KB 证明独立验证区块；切换硬分叉在多节点测试网平稳激活。

**M7b — utility gate：把证明绑定到真实模型（修复 Pearl 核心缺陷，开放研究）**
- 核心难题：如何**强制**矩阵来自真实模型的某层、并验证「有用」而非随机数（Pearl 现网用 `verify_plain_proof` 只验正确性，随机矩阵零成本通过）。
- 设计方向：(1) 把 matmul 输入**承诺**到一个链上注册的模型权重 Merkle 根 + 一笔真实推理请求；(2) ZK 电路额外证明「该 GEMM 对应已承诺模型的指定层、输入为真实请求张量」；(3) 推理结果与证明一起上链/可被需求方取回。`tao-pouw` 实现 utility gate + 模型注册表。
- 参考 Pearl `vllm-miner`（vLLM 插件把量化线性算子替换为 NoisyGEMM）的工程形态，但补上「绑定真实需求」这一层。
- ⚠️ 诚实标注：这是**开放研究问题**，没有真实付费推理需求时仍会退化为「AI 形状 PoW」。需配套真实需求侧（类似 Pearl×Together AI 的折扣推理端点）才成立。先做可行性原型 + 安全分析，再决定是否上主网。
- **验证**：原型能证明「这次挖矿的 matmul 确实是某注册模型对某真实输入的前向计算」，且伪造（随机矩阵）会被 utility gate 拒绝；推理输出正确性对照基线模型。

**M7c — ML 防御层（共识链路之外、纯 advisory，与 PoW 无关）**
- P2P 异常/eclipse 检测、51% 算力突增预警（触发提高确认数）、mempool 垃圾分类、MEV/夹子检测。每节点可跑不同模型，**不参与区块有效性判定**（否则破坏确定性、引入攻击面）。

### M8 —（更后期）blockDAG 共识升级研究
- 与主线解耦的研究轨：GHOSTDAG（blue/red 分类、k 参数、selected parent chain、确定性全序）、高效**可达性查询**、**pruning**——三大硬核。先用 `simpa` 式模拟器在目标出块率下验证参数安全，再考虑把 SVM 接到「DAG 线性化」之后。
- 与 matmul-PoUW 正交：DAG 升级只换共识结构，PoW 算法与执行层不变。

---

## 关键风险与缓解

| 风险 | 缓解 |
|---|---|
| **state_root 状态承诺设计**（账户哈希昂贵/不确定） | MVP 用简单确定性 accounts delta hash；性能/证明需求出现后再上 Sparse Merkle Tree（参考 Solana bank hash / KIP-21 SMT） |
| **recent blockhash 语义**（出块时间秒级 vs Solana 400ms，交易过期窗口） | `BlockhashQueue` 保留足够多块；适当放宽过期窗口，钱包侧透明 |
| **算法硬分叉的低难度攻击窗口** | 切换时重置难度 + 检查点 + 临时放宽 DAA；CPU 阶段尽量短 |
| **RandomX 已被 ASIC 攻破** | CPU 公平启动阶段短期化或定期改参数（Monero 模式） |
| **matmul-PoUW 依赖未充分检验的硬度猜想**（直积矩阵转录） | 排在 MVP 之后；上主网前做独立密码学审计 + 安全分析；保留回退到 kHeavyHash 的选项 |
| **Plonky2 STARK 证明设施复杂、攻击面大** | 复用 Pearl `zk-pow` 久经 CI 的实现而非自写；锁版本 + 回归测试；先小矩阵原型 |
| **「有用功」退化为「AI 形状 PoW」**（无真实需求时同 Pearl 现状） | utility gate 绑定真实模型 + 配套真实推理需求侧；无需求则诚实标注为「AI 形状 PoW」，不夸大宣传 |
| **matmul 锁定 NVIDIA Hopper（Pearl 现状）伤害去中心化** | 移植 `pearl-gemm` 到消费级 GPU（SM≥8.x）+ CPU 回退；矩阵规模分层而非硬件锁定 |
| **blockDAG + SVM 热点账户冲突**（阶段2） | 与 SVM 兼容解耦独立研究；用声明账户锁做静态冲突信息 + 必要时重执行；先模拟器验证 |
| **crate 供应链安全**（曾有针对 Solana 私钥的恶意 crate） | `cargo audit` + 关键依赖 vendoring |
| **SVM API 随 Agave 演进** | 锁定 `solana-svm` 版本；用 `mollusk-svm` 做回归测试 |

---

## 端到端验证（贯穿 MVP）

最终「可用链」验收标准（M0–M6 完成后）：

1. **多节点出块**：3 节点 devnet 稳定挖矿、传播、难度收敛。
2. **Solana 工具直连**：`@solana/web3.js` 指向本链 RPC，完成原生转账；Phantom 钱包接入显示余额并发交易。
3. **SPL Token**：用 Solana CLI 创建 mint / 铸币 / 转账成功。
4. **Anchor 程序**：部署一个最小 Anchor 程序，链侧零改动调用成功。
5. **确定性**：任意两节点对同一区块重放得到一致 `state_root`。
6. **CPU 挖矿**：普通笔记本 CPU 能用 RandomX 参与挖矿。

每个里程碑各自带 `cargo test` 单测 + 集成测试（参考 rusty-kaspa `testing/integration/`），关键共识/难度/执行路径必须有确定性回归测试。
