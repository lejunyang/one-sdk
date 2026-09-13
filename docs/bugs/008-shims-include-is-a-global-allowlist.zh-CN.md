# 008 — `shims.include` 是全局白名单，取回一个命令会禁掉其余所有工具

**状态**：已修复 · **严重度**：严重 · **实测环境**：Windows x64、osdk 0.0.2

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

## 修复

保留 `include` 的既有语义（对「只要这几个」它是正确的），另外补上缺失的那一半。

### 1. 新增 `shims.expose`：增量，不排他

```
osdk config set shims.expose "conda:m2-base:make"
```

它只做加法：把归属规则自行 withheld 的名字放行，从不因此收走别的东西。这正是
006 场景需要的语义 —— 元包不拥有 prefix 里的任何命令，所以 `make` 必须点名索取，
而「点名索取一个」不该意味着「其余都不要」。

`exclude` 仍然最后生效，所以 `expose = ["conda:m2-base:*"]` 配
`exclude = ["conda:m2-base:ls"]` 这种「放开一批、剔掉个别」照样能写。

反过来，`expose` 会**盖过**别处的窄 `include`：两者都命中同一个名字时，一边说
「要这个」，一边说「不在名单里」。让 `include` 赢就等于显式要求被静默丢弃，所以
`expose` 优先。

### 2. 三个列表都可按 tool 独立设置

```
osdk config set shims.conda:m2-base.expose  "make,sh"
osdk config set shims.android-ndk.include   "clang,llvm-strip"
osdk config set shims.conda:m2-base.exclude "ls,test"
```

TOML 落成 `[settings.shims.tools."conda:m2-base"]`。这是让 `include` 变安全的关键：
限定到一个 tool 之后，它的白名单语义只在这个 tool 内成立，`conda:m2-base` 下的
`include` 再也不可能收走 `cargo`。

字段级覆盖：`Some(vec![])`（显式为空）与 `None`（未指定、继承全局）是两回事，所以
只调一个维度不会顺手清掉另一个维度。`config get` 对未指定的字段显示 `inherit` 而不是
空串 —— 两者含义不同，都显示为空会看不出哪个在生效。

### 3. 让归属判定与生成判定用同一个谓词

这是排查中才浮出来的第二层，也是「配置读到了、`published` 也对了，但 shim 就是不落
盘」的真因：`build_bin_ownership_candidates` 里 `.filter(|bin| bin.owned)` 只看
`owned`，**完全不读配置**。于是生成侧按配置写出 `make`，回收侧不认它、随即删掉。

那里的注释当时写着「未拥有的 bin 仍可达：`shims.include` 点名即可」—— 但这个函数从
未接触过任何配置，所以**取回从来就没真正生效过**。008 之所以看起来只是「include 太
宽」，是因为另一半症状被这层掩盖了。

改法是把谓词参数化，路由（`osdk-shim`）、回收（`reconcile_managed_shims`）和生成三
处共用同一个 `shim_is_enabled_for`。三者必须一致：少接一处，就会退化成「shim 存在但
路由不到」或者「生成完又被删」。

### 4. 两条把人带进坑里的提示都改了

`osdk where --bins` 的 "re-add one with ... shims.include" 与 conda 元包告警里的同一句，
现在都指向 per-tool 的 `expose`。原提示是这次事故的直接起因。

## 原修复方向

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

## 实测

`ai-auto-android` 收敛为 `m2-base` + `with` 后，用 per-tool `expose` 取回 13 个命令：

| 观测 | 修复前 | 修复后 |
| --- | --- | --- |
| shims 总数 | 646 -> **0** | 574 -> **600** |
| `cargo` / `go` / `node` | **全部消失** | 保留 |
| `make` / `sh` / `tr` / `awk` | 无 | **全部生成** |
| `ls`（未列入 expose） | — | 不生成（不劫持 Windows `ls`） |
| `make` 端到端 | 只能走 `osdk exec` | **直接调用即可** |

最后一行的实跑输出（recipe 里串联 `tr` / `awk` / `printf`，不经 `osdk exec`）：

```
make: Entering directory '/tmp/maketest2'
HELLO-VIA-SHIM
sum=6
done
make: Leaving directory '/tmp/maketest2'
```

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
