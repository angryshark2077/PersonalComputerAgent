# Security and Privacy

## 1. Product boundary

V0 是设备所有者或明确授权使用者的个人数字活动记录系统，不是员工隐蔽监控、远程控制或账号接管工具。

## 2. Silent operation invariant

静默运行必须同时满足：

1. 用户已经在产品中显式启用该采集类别。
2. macOS 权限已经由用户授予。
3. Agent 只在授权 Scope 内等待、探测、重试和采集。
4. 用户仍可在 Dashboard/设置中查看开关、范围、保留、导出和删除。

禁止把“无感”解释为绕过授权、隐藏采集能力或修改第三方 App。

## 3. Data classes

| Class | Examples | Default |
|---|---|---|
| public/normal | app name, device health, non-sensitive metadata | normal retention |
| medium | window title, URL title, file metadata | scoped, redactable |
| high | screenshots, message body, precise location | explicit enable + retention |
| secret | WeChat KeyMaterial, device token, Bridge secret | Keychain/Secret Store only |

Secret 永不进入 Event Payload、SQLite 普通表、日志、诊断包或对象存储。

## 4. Local controls

- App data directory 0700.
- DB, screenshots and sensitive temp files 0600.
- UDS 0600 with nonce/shared-secret handshake.
- V0 不开放 loopback HTTP。
- Native Messaging 只允许签名/白名单扩展。
- 子进程使用固定路径、参数数组、deadline、kill-on-timeout；禁止 shell interpolation。
- SQLCipher 只读；不在公共 `/tmp` 写完整明文 WeChat DB。
- Keychain 失败时拒绝 Provider 激活，不降级为明文文件。

## 5. Cloud controls

- Better Auth Web Session 与 Device Credential 分离。
- Workspace Scope 由服务端强制校验。
- 对象上传使用短期预签名 URL、hash 和 complete handshake。
- 删除写 Tombstone，离线设备上传旧记录时拒绝复活。
- 日志默认脱敏 message body、window title、URL query、paths 和 identifiers。

## 6. WeChat-specific controls

- 正常流程不退出、不启动、不重签、不注入或修改 WeChat。
- 不发送消息，不自动回复。
- 不读取 Cookie、密码或表单。
- Passive scan 仅在能力探测允许且当前用户已授权时运行。
- 不支持版本进入 `unsupported`，不得通过未知偏移或宽松解析“碰运气”。
- 4.1.12 兼容性必须使用 fixture 和只读验证，不把成功读取一个 DB 误判为全面兼容。

## 7. Security release gate

发布前必须检查：

- secrets scan
- dependency/License audit
- Keychain failure path
- UDS unauthorized client test
- Workspace cross-tenant test
- Tombstone resurrection test
- update signature/notarization
- diagnostic redaction
- provider process timeout
- permission revoke within 5 seconds
