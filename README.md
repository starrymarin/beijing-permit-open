# Beijing Permit Open

Beijing Permit Open 是一个独立开源的北京交警异步 Rust 客户端。它通过北京市统一身份认证完成北京交警登录，当前支持账号查询和进京证业务，包括车辆绑定、办证资料校验、提交申请、查询审核结果和电子证。

接入参数和用户授权由调用方提供，可用接口以上游策略为准。

[![京途小助手，进京证自动续期、外地车导航。微信扫码打开。](assets/jingtu-plush-poster.webp)](https://jing.vibeco.me/intro/)

## 安装

需要 Rust 1.88 或更新版本，以及 OpenSSL 开发库。Linux 可安装 `pkg-config` 和 `libssl-dev`，macOS 可使用 Homebrew 的 `openssl@3`。应用需有 Tokio 异步运行时。

在应用的 `Cargo.toml` 添加 Git 依赖。

```toml
[dependencies]
beijing-permit-open = { git = "https://github.com/starrymarin/beijing-permit-open.git" }
serde_json = "1"
```

如果已在本地克隆仓库，也可以添加路径依赖。

```toml
[dependencies]
beijing-permit-open = { path = "../beijing-permit-open" }
serde_json = "1"
```

## 运行查询示例

在仓库目录执行 `cargo run --locked --example permit-status`。示例从环境变量读取已获授权的会话和业务参数，只打印车辆及办理状态条目数量。

| 环境变量 | 用途 |
| --- | --- |
| `BJ_PERMIT_APP_TOKEN` | 已授权会话的访问令牌 |
| `BJ_PERMIT_STATE` | 进京证业务会话的 `state` 参数 |
| `BJ_PERMIT_SOURCE` | 建立进京证业务会话所用的 `source` 请求头 |

## 北京交警登录

`Login::start(client_id, state)` 打开统一身份认证的北京交警登录入口，并使用库中配置的北京交警授权回调地址。下例采用北京交警 App 的应用标识 `100100001423` 和登录事项参数 `100009`。

每次交互新建一个 `Login`。调用 `captcha()` 后把 `bytes` 按 `mime_type` 显示给账号本人，传入其填写的答案。

```rust
use beijing_permit_open::Login;

// 密码登录
let login = Login::start("100100001423", "100009").await?;
let captcha = login.captcha().await?;
// 显示 captcha 并获取本人输入
let session = login.login_with_password(username, password, captcha_answer).await?;
```

```rust
// 短信登录
let mut login = Login::start("100100001423", "100009").await?;
let captcha = login.captcha().await?;
// 显示 captcha 并获取本人输入
login.send_sms(mobile, captcha_answer).await?;
// 本人收到短信后输入验证码
let session = login.login_with_sms(sms_code).await?;
```

身份认证成功后，库会用授权回调换取北京交警会话 `AppSession`。密码登录是否可用取决于账号及上游策略。遇到额外身份核验或官方客户端要求时，库会返回 `Error::OfficialClientRequired`。

### 使用已有会话

已有北京交警会话时可用 `AppSession::from_token` 接入，调用 `session.account().await?` 核对账号。返回的账号信息包含个人资料。

## 进京证查询和申请

将 `AppSession`、进京证业务的 `state` 和 `source` 传给 `PermitClient::new` 即可建立进京证业务客户端。这里的业务 `state` 由进京证服务使用，登录入口的 `100009` 用于统一身份认证。下例按号牌和号牌种类查询六环内进京证的可办日期。

```rust
use beijing_permit_open::{PermitClient, PermitKind};

async fn available_dates(
    permit: &mut PermitClient,
    plate: &str,
    plate_type: &str,
) -> beijing_permit_open::Result<Option<serde_json::Value>> {
    let vehicles = permit.vehicles().await?;
    let Some(vehicle) = vehicles.iter().find(|v| v.plate == plate && v.plate_type == plate_type) else {
        return Ok(None);
    };
    permit.check_vehicle(vehicle).await?;
    let dates = permit.available_dates(vehicle, PermitKind::InsideSixthRing).await?;
    Ok(Some(dates))
}
```

进京证办理流程如下。

1. 调用 `states` 从 `bzclxx` 取得申请状态及车辆可办证型。通过 `vehicles` 核对车辆归属。需要绑定新车时，由调用方提供 `VehicleRegistration` 字段，再调用 `add_vehicle` 并回查列表。
2. 调用 `check_vehicle` 核对办证资格。用 `configuration` 和 `driver` 取得配置及驾驶员资料，按配置收集同行人资料后传给 `check_driver`。人员校验用 `jsrxm`、`jszh`、`dabh`、`rylx`、`txrxx`、`txrkg`、`wtxr`。保留上游返回的 `rylx`，缺省时库会在人员校验请求中填入 `"0"`。
3. 用 `available_dates` 的 `jjrqs` 选择日期，从 `purposes`、`districts` 和 `roads` 选择官方字典项。用 `route_requirement` 获取是否填写拟行驶路线的上游值。地图分支使用用户确认的地点和原始坐标。
4. 由申请人核对最终字段。用 `Application::from_parts` 合并官方车辆条目、人员资料和出行字段，也可将完整的确认 JSON 交给 `Application::from_official_fields`。持久化本次提交意图和提交前记录基线，再显式调用一次 `submit_application`。以下字段表可用于组装表单。
5. 按车辆、证型、生效日和记录基线匹配新增的官方申请记录。提交响应表示受理，审核结果以 `application_detail` 的 `blzt` 为准。`application_records` 按北京月份查询申请记录，输入格式为 `YYYY-MM`；详情和电子证使用同一条记录中的 `apply_id` 或 `applyId` 及原始 `sqsj`。

```rust
use beijing_permit_open::Application;

let application = Application::from_parts(
    official_vehicle_state, // states()["bzclxx"] 中与所选车辆匹配的条目
    PermitKind::InsideSixthRing,
    checked_driver_fields, // check_driver 已通过的输入资料
    reviewed_trip_fields,  // 下表中日期、目的、目的地、道路等字段
)?;
let receipt = permit.submit_application(application).await?;
```

客户端的网络层禁用自动重试，写入后超时、取消或返回 `Error::OutcomeUnknown` 时，调用方应先查官方记录。重启后也应先处理未确认的持久化意图，避免再次提交同一事项。

## 最终申请字段

| 字段 | 来源 |
| --- | --- |
| `vId`, `hphm`, `hpzl`, `cllx` | 官方所选车辆。保留 `vId` 的原始 JSON 标量类型 |
| `ylzsfkb`, `elzsfkb`, `ylzqyms`, `elzqyms`, `ylzmc`, `elzmc` | `states` 的车辆条目，保留官方返回值 |
| `jjzzl`, `jjrq` | 证型字符串 `"01"` 六环内或 `"02"` 六环外，以及 `jjrqs` 中选中的日期 |
| `jsrxm`, `jszh`, `dabh`, `rylx`, `txrxx` | 人员校验通过的资料。`txrxx` 为数组，每项用 `txrxm`、`txrsfzh`、`txrsjh` |
| `jjmd`, `jjmdmc` | 进京目的的编码和名称，多项分别用英文逗号拼接 |
| `area`, `jjdq`, `xxdz`, `jjdzgdjd`, `jjdzgdwd` | 目的地区名、编码、地址和可用的目的地坐标 |
| `sfzj`, `jjlk`, `jjlkmc`, `jjlkgdjd`, `jjlkgdwd` | 外埠进京时用 `sfzj: "0"`，填写字典道路或地图道路 |
| `sfzj`, `zjxxdz`, `zjxxdzgdjd`, `zjxxdzgdwd` | 已在京时用 `sfzj: "1"`，填写车辆在京位置 |
| `sqdzgdjd`, `sqdzgdwd`, `jingState` | 当前申请位置及官方表单状态值，按实际流程填写 |
| `xslx` | 官方要求时传拟行驶路线。值是编码成 JSON 字符串的地点名称数组 |

## 已封装的上游操作

| Rust 方法 | 上游 |
| --- | --- |
| `Login::start`, `captcha`, `send_sms`, `login_with_sms`, `login_with_password` | 统一身份认证的 `goUserLogin`、`checkH5LoginPermit`、`generateCaptcha`、`sendSmsBeforeLogin`、`doUserLoginBySms`、`doUserLoginByPwd` |
| 登录成功后建立 `AppSession` | 北京交警的 `auth/appLogin` |
| `AppSession::account` | `system/user/getInfo` |
| `PermitClient` 建立业务会话 | `auth/getSsoUserToken` → `auth/userController/loginUserByUserCenter` |
| `vehicles`, `vehicle`, `add_vehicle` | `vehicleController/getUserIdInfo`、`vehicleController/getInfo`、`relationController/add` |
| `states`, `configuration` | `applyRecordController/stateList`、`configRecordController/getConfigRecordInfo` |
| `driver`, `check_driver`, `check_vehicle` | `getJsrxx`、`applyCheckNum`、`applyVehicleCheck` |
| `available_dates`, `route_requirement`, `purposes`, `districts`, `roads` | `checkHandle`、`checkInputRoadInfo`、`ucDicController/queryDic` |
| `submit_application`, `application_records`, `application_detail`, `electronic_permit` | `insertApplyRecord`、`applyRecordList`、`applyDetail`、`cardInfoById` |

## 验证

```sh
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo package --allow-dirty
```

测试使用本地模拟服务和合成资料，覆盖两种登录、验证码会话、RSA 负载、业务会话交换、车辆绑定、人员和车辆校验、提交及超时类结果、申请列表分页和审核查询。此版本的直连提交尚待真实官方业务验收。
