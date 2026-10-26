# Modbus TCP

[English](README.md) · [简体中文](README.zh-CN.md)

一个 DBX 插件，用于读写工业设备上的 **Modbus TCP** 数据：PLC、网关与远程 IO 耦合器。

基于官方 `dbx-plugin create` 的 **Rust + 前端** 模板构建。

- [English plugin development guide](https://dbxio.com/en/docs/plugin-development)
- [中文插件开发文档](https://dbxio.com/cn/docs/plugin-development)

## 提供的贡献点

| 贡献点 | 类型 | 用途 |
| --- | --- | --- |
| `io.github.bobocha214.modbus.modbus` | connection-provider | Modbus TCP 连接表单 |
| `io.github.bobocha214.modbus.modbus.workbench` | workbench | 带轮询与受控写入的寄存器表格 |
| `io.github.bobocha214.modbus.mcp` | mcp | 供 DBX AI 助手使用的只读工具 |

工作台由 `ui/index.html` 提供，页面按 `dbxPlugin.contributionId` 路由。

## 界面

工作台遵循同样的布局：**配置在左，值在右。** 描述「读什么」的东西不会挡在你正在读的数据前面，
所以表格永远不用和表单抢空间。

样式使用 DBX 自己的设计令牌（`--color-background`、`--color-card`、`--color-muted`、
`--color-border`、`--color-primary`、`--radius-md`/`-lg`），因此插件会跟随宿主应用的亮色与暗色主题，
而不是自带一套观感。没有任何地方写死白色背景。

**Modbus TCP**

- **创建连接时只需要填一个工作区名称，别的什么都不问。** 不要 IP、不要端口、不要从站号：
  Modbus 工作区只是一个容器，每个地址都是你在里面添加的一台设备。这样宿主的连接对话框就不会
  逼你填写还没查到的值，而一个不带地址的连接会明确说明这一点，而不是去拨一个占位地址。
  字节序与字序同理，它们属于「怎么读这些数据」，所以同样放在工作台里。
- **侧边栏有两个页签。** `设备` 管理端点，`范围` 管理从端点读什么。
- **设备。** 每台设备是一个 IP、端口、从站号和超时，在后端拥有独立会话，所以一个工作台可以同时
  读多台 PLC。每台设备都会显示自己的状态——`已连接` 并给出实测往返时间、`连接中`、
  `不可达` 并带上后端给出的原因，或 `未连接`；连接失败的设备会以递增且有上限的延迟重试，
  而不会被默默当成在线。仍被某个范围引用的设备不能删除。
- **工作台自己管理连接。** 打开它就够了：保存的连接只用来播种第一台设备的地址，而完全不带连接
  打开的工作台同样可用，头部显示 `standalone`。在设备还没有地址之前，内容区显示的是一张引导卡，
  点击即跳到表单，所以你做的第一件事是填 IP，而不是盯着空表格。没有地址的设备绝不会被拨号，
  也绝不进入重试循环；它会一直保持 `未连接`，直到你填上地址。
- **范围。** 每条记录都是一个连续范围——设备、区域、起始地址、数量——并带有自己的数据类型、
  字节序、字序、显示格式与扫描周期。新建范围默认读 **10 个地址**，正好是一般值得盯着的一块；
  数量、起始地址与区域之后都能改。选中一条即加载它：网格、表单和解码读数都跟随选择。
  用 `+` 添加范围（新范围从上一个结束处往后接），在行内删除。最后一条范围不能删除。
- **改定义当场生效。** 修改设备、区域、起始地址、数量或数据类型会自己发起一次新读取——不需要再按
  一次「读取」或「轮询」；正在跑的轮询也不会被打断，它会在排队的那一次里读新定义。如果换了更宽的
  数据类型（每个值占更多寄存器），会顺带把缺的那些字取回来，而不是让网格空掉一半。设备侧的编辑
  也是同一规则：改 IP、端口或超时会关掉会话并按新地址重拨，改从站号由下一次读取直接生效。
- **右侧只有地址和值**——一个地址一格，包含地址、值以及未经处理的原始字，并用等宽十六进制列对齐。
- **列数与行数决定一次能看到多少。** `列数`（1–6）是一行放几格，`行数`（1–50）是一页放几行，
  两者相乘就是一页容量。超过一页的范围用 `‹ 1 / 2 ›` 翻页而不是滚动，这样轮询时屏幕上的地址数量
  始终不变。两者都是视图设置：改它们只会重排网格并保留已读到的值，不发请求。
- **`日志` 按钮打开读取日志。** 每次读取都会记下时间、设备、地址范围、结果与耗时。相同结果会折叠成
  一行并计数，所以一秒一次的轮询不会把其他事件冲掉，而新出现的失败仍然单独占一行。失败的读取会
  记下确切原因——Modbus 异常码、超时、连接被拒绝——并把同样的原因显示在网格上方，无论日志是否
  打开都看得见。
- **双击数值即可写入。** 单元格就地变成输入框：`Enter` 发送、`Esc` 取消，文本没改就不写，所以误双击
  是无害的。线圈单元格接受 `true`/`false`。只读区域（输入寄存器、离散输入）不可编辑，与后端规则一致。
- **字节序、字序与显示格式只影响解码后的值。** 每格的原始字始终显示真正从线上收到的 16 位寄存器；
  改这些选项只会重新解码并重绘网格，不额外发请求。字序对窄于 64 位的类型会禁用，因为它们本来没有
  高低字之分。
- 头部的 `Tx`（已发请求）与 `Err`（失败响应）紧挨着「读取」和「轮询」按钮。
- 侧边栏与网格之间的分隔条可拖动，也可以用方向键调整。

工作台在头部报告连接状态。写入永远是「显式编辑某一格」的结果，并由提示条确认，提示里写明地址和
实际发送的值；被拒绝的写入会显示后端给出的原因并把单元格恢复原样，而不是留着用户输入的值。

## Modbus TCP 协议实现

直接在 `backend/src/modbus.rs` 中实现——Modbus 应用协议跑在 MBAP 之上，依赖树里没有任何驱动 crate。

支持的功能码：`01` 读线圈、`02` 读离散输入、`03` 读保持寄存器、`04` 读输入寄存器、
`05` 写单个线圈、`06` 写单个寄存器、`15` 写多个线圈、`16` 写多个寄存器。

寄存器解码支持 `uint16`、`int16`、`uint32`、`int32`、`float32`、`uint64`、`int64` 与 `float64`，
并支持四种经典字节序：

| 字节序 | 含义 |
| --- | --- |
| `ABCD` | 大端，Modbus 的默认顺序 |
| `BADC` | 每个 16 位字内部交换字节 |
| `CDAB` | 每个 32 位组的高低 16 位对调 |
| `DCBA` | 32 位组完全反转 |

`word_order`（`big` / `little`）会先反转寄存器顺序，这对跨四个寄存器的 64 位值才有意义。

TCP 连接在多次读取之间复用，并且在设备悄悄断开空闲连接时自动重连一次。Modbus 异常响应按名称
报告（例如 `Modbus exception 2 (illegal data address)`）。

## 连接生命周期

`connection/test`、`connection/connect` 与 `connection/disconnect` 把结果放在返回体的
`{ "success": true|false, "message": "..." }` 里，而不是抛 JSON-RPC 错误——对两种连接类型都是如此。
设备不可达或超时是连接表单要显示的情况，不是插件故障。`message` 里带着底层原因，例如
`Modbus TCP unreachable on 192.168.1.10:502: connection refused`。

只有缺失 `connection` 负载、未知连接类型或未知方法才返回 JSON-RPC 错误。`connect` **之后**的调用
（`modbus/read` 等）用裸 `connectionId` 引用会话，因此它们用的是另一套 id 解析函数——生命周期调用
带的是完整连接快照。

### 连接负载的字段分桶

DBX 会把保存的连接序列化到三个桶里，`manifest.json` 中的字段绑定决定某个值落在哪个桶：

| 桶 | 绑定 | 示例字段 |
| --- | --- | --- |
| 顶层 | `name`、`host`、`port`、`username`、`password`、`database` | `host`、`port` |
| `external_config` | `config` | `unit_id`、`byte_order` |
| `connection_secrets` | `secret`（`password` 字段的默认绑定） | `password` |

提供者类型**不在** `database_type` 里——那个键对任何插件连接都硬编码为 `"plugin"`。真正的值以
`plugin_connection_type` 到达（即贡献点声明的 `database_type`），贡献点 id 则在
`plugin_connection_provider` 里。`provider_of` 按这个顺序读取，只有都缺失时才回退到旧写法
`database_type` / `databaseType` / `dbType`，所以宿主侧改名不会悄悄变成
`Unsupported connection type ""`。

因为密钥单独放在一个桶里，字段查找会依次查顶层、`external_config`、`connection_secrets`。只读顶层
会静默丢掉密码，因为密钥从来不会作为普通配置回传。

## AI 工具

sidecar 实现了 `mcp/tools` 与 `mcp/call`，只有一个只读工具：`modbus_read`。需要在
**插件中心 → 已安装 → 内置 AI 工具** 里为这个插件单独开启；只安装插件不会暴露任何工具。

## 开发

装好 Node.js 22+ 与 Rust 后，可以不开 DBX，直接跑浏览器开发宿主：

```bash
dbx-plugin dev --path . --port 5190
```

CLI 会编译 Rust 后端。开发凭据以明文形式存放在本地 `.dbx-dev/` 下，请勿把该目录提交或打包。
最终集成测试请使用真实的 DBX 宿主——开发宿主只实现了 Host API 的一个子集。

跑后端单元测试（Modbus 字节序与寄存器编码）：

```bash
cd backend && cargo test
```

构建可安装的候选包：

```bash
dbx-plugin package .
```

该命令会为当前宿主构建原生后端，收集 `manifest.json`、`assets/` 与 `ui/`，然后把未签名的、
针对特定目标的 `.dbxp` 候选包以及对应的 `.artifact.json` 写入 `dist/`。

> sidecar 二进制是目标相关的，所以插件按操作系统与 CPU 架构分别出包，而不是一个
> `universal` 包。因此本地构建永远只会产出**一个**目标——你正在运行的那个。
> 参见 [多平台构建](#多平台构建)。

## 多平台构建

Rust sidecar 是原生二进制，每个操作系统与 CPU 架构都需要各自构建。`dbx-plugin package` 在
**它运行的那个宿主上**编译后端，不做交叉编译，`--target` / `DBX_PLUGIN_TARGET` 只是给产物打标签。
在 Windows 上构建永远出不了 `linux-x64`，传什么 target 都一样。

所以多平台覆盖靠「每个平台各构建一次」，由 `.github/workflows/` 里的两个工作流用 runner/target
矩阵完成：

| 目标 | Runner | 说明 |
| --- | --- | --- |
| `linux-x64` | `ubuntu-24.04` | |
| `linux-arm64` | `ubuntu-24.04-arm` | ARM runner 需要套餐支持 |
| `windows-x64` | `windows-2022` | |
| `darwin-x64` | `macos-15-intel` | 上游正在退役 Intel macOS runner |
| `darwin-arm64` | `macos-15` | Apple silicon |

- **`.github/workflows/plugin-build.yml`** 在推送到 `main`、PR 和手动触发时构建全部目标，
  每个目标上传一个 `.dbxp` 及其 `.artifact.json` 作为工作流产物。它同时充当可移植性门禁：
  平台相关的编译错误在这里就会暴露，而不是等到发布时才炸。
- **`.github/workflows/plugin-release.yml`** 在 GitHub Release 发布时跑同一套矩阵，并把每个目标
  加上合并后的 `release-candidates.json` 挂到该 Release 上。

如果某个 runner 你的账号用不了，只删掉对应的矩阵条目即可——其余目标照样构建，插件能在你发布过的
目标上安装。要支持 Windows on ARM，在两个矩阵里都加上
`{"runner":"windows-11-arm","target":"windows-arm64"}`。

后端本身是平台中立的：`backend/src` 只用 `std::net`、`std::sync`、`std::time` 与 `serde_json`，
没有 `#[cfg(...)]` 分支，也没有 Windows 专属 API；`dbx-plugin.toml` 声明
`binary = "dbx-plugin-modbus"` 且不带扩展名，由打包器在 Windows 上补 `.exe`。

## 验收清单

- 空配置、错误凭据、超时、设备不可达以及 sidecar 重启，都要表现为可读的错误而不是卡住。
- DBX 的亮色与暗色主题、`zh-CN` 与 `en`。
- 重连、关闭工作台、重复打开同一个工作台，以及完全不带连接打开工作台——先出引导卡，然后在里面
  配置设备并连接。
- 打包出的 `.dbxp` 里不含任何开发数据或凭据。

## 发布

1. 发布一个 GitHub Release。工作流会为每个目标构建未签名候选包。
2. 如果本仓库已登记 `autoUpdate: true`，DBX 商店会自动创建或更新候选 PR；否则手工向
   **`t8y2/dbx-store:main`** 提一个候选 PR，附上 Release 与 `release-candidates.json` 的地址
   （不需要再开提交 Issue）。
3. 审核通过后，DBX 商店用官方仓库密钥为候选包签名，并在同一个 PR 里补上可安装资产与签名元数据。

不要把普通的插件源码提到 `t8y2/dbx`；那个仓库只接受插件宿主、SDK、CLI、schema、文档与官方示例的改动。
