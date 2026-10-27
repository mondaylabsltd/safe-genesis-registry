# Safe Genesis Registry（中文说明）

一个中立、无需许可、只写一次的链上登记表：记下一个 Safe **是怎么被创建的**。每个 Safe 只有一条记录，按 Safe 自己的地址查询，用来在任何其他链上以同一地址再部署一次。没有 owner、管理员、升级、费用，没有外部调用，也不需要运行任何服务。Vela 是它的第一个用户，不是它的所有者。

## 要解决的问题

Safe 的地址永远由**创建调用**决定（factory、singleton、`setup()` initializer、saltNonce），而 owner 会变。改过成员之后，链上读到的只是"现在谁控制它"，读不到"它当初是怎么创建的"。要在新链上部署同一个 Safe，需要逐字节相同的原始创建参数。这些参数在链上没有可直接读取的形式：它们埋在历史交易里（常常还嵌在 4337 bundle 或 MultiSend 里），或者在某个钱包的本地存储、某个链下服务里。

本合约按 Safe 地址把它们存下来，一次 `eth_call` 就能读到。

## 接口

```solidity
// 写：Safe 地址由合约自己算，调用方不传
record(address factory, bytes proxyCreationCode, address singleton, bytes initializer, uint256 saltNonce)
    returns (address safe, bool created)

// 读：按 Safe 地址查
genesisOf(address safe)    returns (bool found, Genesis genesis)
deployCallOf(address safe) returns (address factory, bytes data)  // 把 data 发给 factory 即可部署
isRecorded(address safe)   returns (bool)
computeAddress(factory, proxyCreationCode, singleton, initializer, saltNonce) returns (address)
```

`Genesis` 包含以下字段：`factory`、`singleton`、`saltNonce`、`initializer`（即 `setup(...)` 调用，里面有初始成员、阈值、fallback handler、setup hook），`proxyCreationCodeHash`，以及仅作参考的 `recorder` 和 `recordedAt`。

## 为什么一个 Safe 只对应一条 genesis

合约完全按 Safe proxy factory 的 `createProxyWithNonce` 公式自己推出地址（从 v1.1.1 到 v1.4.1 都是这个公式）：

```
salt         = keccak256(keccak256(initializer) ‖ uint256(saltNonce))
initCodeHash = keccak256(proxyCreationCode ‖ uint256(uint160(singleton)))
safe         = address(keccak256(0xff ‖ factory ‖ salt ‖ initCodeHash)[12:])
```

存下的每一个字段都参与了地址计算。singleton、initializer、saltNonce、factory、proxy 代码中任何一项不同，算出来的都是另一个地址。所以某个 Safe 地址下面，只可能存着它自己的 genesis；两组参数要撞到同一个地址，等于找到 keccak256 碰撞。由此得出：

- **每个 Safe 一条记录，只写一次。** 任何人都能写，部署前后都行。谁都改不了、覆盖不了，也没法用别的内容抢占一个已存在 Safe 的地址（那需要 160 位的地址原像）。重复写入返回 `created = false`。
- **记录能证明什么、不能证明什么。** 它证明这组参数发给 `factory` 就是这个地址的 CREATE2 输入；它**不能**证明 `factory` 上就是 Safe 官方的 factory，也不能证明重放出来的是同一个钱包——另一条链上同一地址可能是别的代码（恶意 factory 可以返回正确的 proxy 代码、部署到正确地址、却用它自己的 setup）。所以重放前必须在目标链上核对每个依赖合约的代码哈希，部署后再核对完整配置。
- **对于还没人部署过的 Safe**，同时选定两组参数的一方，理论上能用约 2^80 的生日搜索让两种创建共用一个地址。只给你自己生成参数的未部署 Safe 转钱。

`proxyCreationCode` 只做哈希，不存储，因为同一个 factory 版本的所有 Safe 用的都是这同几百字节。传错了，记录只会落到一个不存在 Safe 的地址下面。

## 范围与限制

- **支持的 Safe 版本：** 所有用这个公式推导 salt 的 factory 都支持。factory、proxy 代码、singleton 都是参数，不是写死的常量。如果将来 Safe 的 factory 改了 salt 公式，就部署一个新版 registry 合约；本合约里已有的记录依然有效。
- **`createProxyWithCallback`：** 它创建的 Safe，等同于 `saltNonce = uint256(keccak256(saltNonce ‖ callback))` 的 nonce 形式 Safe。按这个形式记录即可。重放会部署出同一个 Safe、同样的 setup，只是不会调用 callback。
- **`createChainSpecificProxyWithNonce` 不支持：** 它在每条链上的地址都不同，没有可以跨链带走的东西。
- **初始配置不等于当前配置。** 从 genesis 部署出来的 Safe 带的是**初始成员**。源链上改过成员的话，新链上的副本仍是旧成员，直到通过 Safe 自己的交易同步过去。在有人往那条链转钱之前，钱包必须先把这一点告诉用户。
- **只支持 EVM 标准的 CREATE2。** zkSync Era 这类地址推导方式不同的链不在范围内。
- `initializer` 最长 8 KiB（写满约 700 万 gas，低于单笔交易 1678 万的上限）。factory 或 singleton 为零地址、输入为空的写入都会被拒绝。
- v1.0.0–v1.3.0 的 factory 不检查 singleton 是否存在：在缺少 singleton 的链上，任何人都能先部署一个未初始化的 proxy，singleton 出现后谁先调 `setup` 谁就拥有这个地址。这类链对该 Safe 视为不安全。

## 钱包怎么用

- **创建 Safe：** `record` 它的 genesis 即可，不需要部署。从这一刻起它在每条链上的地址都确定了，某条链上的第一笔交易可以用 `deployCallOf` 顺带部署它。还没在任何链上部署的 Safe，只要一个 owner 用 passkey 发起请求（`ownerProof`，见 [API](genesis-cf/README.md)），服务就会代付写入。
- **别处创建的 Safe**（比如 Safe{Wallet}）：只需从创建交易或交易服务里取一次创建参数，然后 `record`。之后谁都不再需要那个来源。如果 `record` 返回的地址和这个 Safe 不一致，说明参数错了。
- **扩展到新链：**
  1. 在任意一条有记录的链上调 `deployCallOf(safe)`。
  2. 在目标链上核对 factory、singleton、fallback handler、每个 setup hook 及其调用/启用的合约的**运行时代码哈希**都是该角色的已知代码，且 `keccak256(factory.proxyCreationCode())` 等于记录里的 `proxyCreationCodeHash`；解不全的 setup hook 一律拒绝。服务的 `GET /api/replay` 就做这一步。
  3. 是合约的 owner 必须绑定在它自己的 genesis 上（或两条链上代码相同），否则谁能在目标链上那个地址部署合约，谁就多一票。
  4. 把 `data` 发给 `factory`（先模拟），部署后读回 owners、阈值、模块、fallback handler、guard，与 initializer 对照。也可以在目标链上再 `record` 一次。
  5. 先把成员同步到当前配置，再把这条链上的地址展示为可以安全收款。

gas 开销（用仓库里的测试夹具：3 个成员，initializer 约 420 字节）：首次写入约 38.7 万，重复写入约 3.4 万，写满 8 KiB 约 700 万。主要花在存储上，在 L2 上很便宜，在以太坊主网上比较明显。

部署方式和测试命令见 [README.md](README.md)。

2026-10-27 的三份对抗性审查（合约、规则层、服务外壳）及修复见 [docs/review-2026-10-27.md](docs/review-2026-10-27.md)。
