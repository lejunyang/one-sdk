# 008 — `shims.include` 是全局白名单，取回一个命令会禁掉其余所有工具

**状态**：待修复 · **严重度**：严重 · **实测环境**：Windows x64、osdk 0.0.2

## 现象

006 修好元包命令归属后，`conda:m2-base` 的 257 个命令全部 withheld，这是正确的。
006 的告警也给出了取回单个命令的方法：

```
Expose one explicitly with `osdk config set shims.include ...`
```

按这个提示操作，结果是**所有工具都不见了**：

```
$ osdk config set shims.include make,sh,bash,tr,awk,...   # 13 个
$ osdk reshim
regenerated 13 shim(s)

$ ls <shims>/ | wc -l
0                      # 原本 646 个，现在一个都没有
```

`cargo`、`go`、`node` 全部消失 —— 它们与这个 conda 包毫无关系。而且报告说
"regenerated 13"，实际连这 13 个也没落盘，目录直接归零。

改用 backend 限定形式（`conda:m2-base:make` 而不是裸 `make`）后，conda 那一侧
正确了，但误伤依然存在：

| 工具 | published | withheld |
| --- | --- | --- |
| `conda:m2-base` | 13（正确） | 244 |
| `go` | **0** | **2（`go`、`gofmt`）** |

`go` 从来不在 include 里，也不是 conda 的东西，却被判为不该有 shim。

## 根因

`crates/osdk-core/src/shim/mod.rs` 的 `shim_is_enabled_for`：

```rust
let included = matches_any(&settings.include);
if !settings.include.is_empty() && !included {
    return false;
}
```

**`include` 一旦非空，就变成对全体工具生效的白名单**：任何没被列出的名字，无论属于
哪个 backend、是否 owned，一律不生成 shim。列 13 个 conda 命令，等于同时声明"其余
633 个工具都不要"。

这个语义本身是有意设计的（文档也说了 include 用于"收窄"），它与 006 的用法冲突：

- **006 需要的**是"在默认的归属判定之外，额外取回几个被 withheld 的命令"——一个
  相对于默认值的**增量**；
- **`include` 提供的**是"只有这些名字可以有 shim"——一个**绝对**的全集替换。

006 的告警把后者当成前者来推荐，于是照做的人会把自己的工具链全部关掉。

至于"报告 13 实际 0"，是另一层问题：reshim 先按新规则删掉不该存在的，再生成该存在
的，但那 13 个 conda 命令在 `owned == false` 时仍走不到生成分支（`!owned && !included`
里 `included` 对裸名匹配成立、对 `conda:m2-base:` 限定形式又要求另一套匹配），最终
删得干净、写得一个不剩。

## 为何严重

- **照文档操作就会踩中**，而不是需要误用。006 的告警是用户遇到问题时唯一的指引。
- **后果远超预期范围**：改一个 conda 包的暴露策略，结果 `cargo` 不见了。与 007 是同
  一类失效模式 —— 局部配置产生全局后果。
- **看起来像成功**：命令返回 0，还告诉你 "regenerated 13 shim(s)"，只有去数目录才
  会发现是 0。
- **恢复方式不直观**：得先想到是 `include` 的问题、`unset` 它、再在**项目外**跑一次
  reshim（项目内跑仍会读到项目配置）。

## 修复方向

1. **分开这两个语义**。`include` 保留"全局白名单"的既有含义，另加一个表达增量的设
   置（例如 `shims.expose`），语义是"在默认判定之上额外放行这些名字"，不影响任何
   未列出的工具。006 的告警改指向后者。
2. **或者让 backend 限定的模式只作用于该 backend**。`conda:m2-base:make` 这样的模式
   带有明确的作用域，把它当成全局白名单条目是违反直觉的；作用域外的工具应当不受影响。
3. **让 include 与 owned 的交互对得上**。取回一个 `owned == false` 的命令是这个设置
   存在的理由，必须真的能生成 shim，而不是删掉之后什么都不写。
4. **reshim 的报告要与落盘一致**。"regenerated N" 应当反映实际写成功的数量；数字对
   而目录空，会让人以为问题在别处。
5. **加一道安全网**。一次 reshim 若要删掉的 shim 数量占现存的绝大多数，值得先提示再
   执行——这次是 646 → 0，属于典型的"配置写错了"而不是"用户真想这样"。

## 回归防线

- 设 `shims.include` 只含某个 conda 包的命令，断言**另一个 backend**（如 `go`）的
  shim 仍然生成。这条修复前必红。
- 断言取回一个 `owned == false` 的命令后，该 shim **确实存在于磁盘上**，而不只是
  `where --bins` 说 published。
- 断言 reshim 报告的数量等于目录内实际文件数的变化。
- 覆盖三种写法：裸名 `make`、backend 限定 `conda:m2-base:make`、带通配 `*`，三者语
  义必须各自明确且互不串味。

## 备注

发现路径值得记一笔：它不是审查代码看出来的，而是在按 006 自己写的告警去解决"项目
需要 make 但它被 withheld"这个实际问题时撞上的。**一条告警把用户导向了一个会造成更
大破坏的操作**，这比缺陷本身更值得警惕 —— 修 008 时要连带确认 006 的告警文案。

与 005 / 006 / 007 一起看，这是本批第四个"局部原因、全局后果"的缺陷。前三个分别出在
DLL 实例化、归属判定和版本偏斜上，008 出在配置语义上。共同点是：**某个机制的作用域
比它的名字暗示的要大**。
