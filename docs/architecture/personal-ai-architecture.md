---
type: Architecture
title: "JARVIS型パーソナルAI構成図"
description: "Galaxyスマホをセンサー／手足、常時稼働サーバーを脳として構成するOSS中心のパーソナルAIアーキテクチャ。"
tags:
  - ai-agent
  - jarvis
  - android
  - openclaw
  - home-assistant
  - termux
  - shizuku
  - mcp
  - letta
generated:
  by: "GPT-5.6 Sol"
  at: "2026-09-27T23:31:00+09:00"
status: superseded
---

> **Status: superseded.** This document predates ADR-0001 and is kept for
> historical context. The current architecture is defined in
> [ADR-0001](../../adr/0001-unified-architecture.md) and the current
> infrastructure/deployment policy in
> [ADR-0004: Local-only (no OCI)](../../adr/0004-local-only-no-oci.md).
> References to OpenClaw and OCI-era assumptions describe the earlier design;
> OpenCode V2 is the current agent runtime and current operations are local
> Kubuntu + systemd (OCI unused).

## 目的

私生活を常時ポーリングするのではなく、スマホ・メール・予定・
スマートホームなどから発生するイベントを集約し、
AIが必要なときだけ通知・提案・自動処理する
JARVIS型パーソナルエージェントを構築する。

基本方針は次のとおり。

- **スマホ**: センサー、通知、位置、UI操作などの「目と手」
- **JARVIS Server**: 推論、判断、記憶、スケジュールの「脳」
- **Home Assistant**: 家・IoT・スマホ状態を集約するイベントバス
- **MCP**: 各システムをAIへ公開する標準インターフェース
- **LLM**: GPT、Claude、Gemini、Qwenなどを交換可能にする
- **危険な操作**: 原則として承認を要求する

## 全体構成

```mermaid
flowchart TB
    subgraph Mobile["Android / Galaxy S26 Ultra"]
        HAC["Home Assistant Companion"]
        Termux["Termux"]
        Shizuku["Shizuku / rish"]
        DroidMCP["droid-mcp / Android MCP"]
        Zafiro["Zafiro<br/>任意: モバイルAI UI"]
        AndroidAPI["Android OS / APIs"]
        Apps["各種Androidアプリ"]

        HAC --> AndroidAPI
        Termux --> AndroidAPI
        Shizuku --> AndroidAPI
        DroidMCP --> Termux
        DroidMCP --> Shizuku
        Zafiro --> DroidMCP
        Shizuku --> Apps
    end

    subgraph Home["自宅"]
        HA["Home Assistant"]
        IoT["SwitchBot / センサー / 家電"]
        IoT --> HA
    end

    subgraph Server["JARVIS Server<br/>OCI / VPS / 自宅サーバー"]
        Gateway["OpenClaw<br/>常駐エージェント"]
        Scheduler["Scheduler / Heartbeat"]
        Policy["Policy Engine<br/>通知・承認・自動実行"]
        Memory["Long-term Memory<br/>Letta / DB"]
        MCPHost["MCP Client / Tool Router"]
        Audit["Audit Log"]

        Scheduler --> Gateway
        Gateway --> Policy
        Gateway <--> Memory
        Gateway --> MCPHost
        Policy --> Audit
    end

    subgraph Cloud["外部サービス"]
        Gmail["Gmail"]
        Calendar["Google Calendar"]
        Web["Web / 天気 / 交通 / 価格"]
        Finance["家計・金融サービス"]
        Other["その他API / SaaS"]
    end

    subgraph Models["LLM"]
        GPT["OpenAI"]
        Claude["Claude"]
        Gemini["Gemini"]
        Qwen["Qwen / OpenRouter"]
    end

    HAC -->|状態・通知・位置| HA
    HA <-->|MCP / REST / Webhook| MCPHost
    DroidMCP <-->|MCP| MCPHost

    Gmail <-->|API / Connector| MCPHost
    Calendar <-->|API / Connector| MCPHost
    Web <-->|HTTP / Browser| MCPHost
    Finance <-->|API| MCPHost
    Other <-->|API / MCP| MCPHost

    Gateway <-->|Model API| GPT
    Gateway <-->|Model API| Claude
    Gateway <-->|Model API| Gemini
    Gateway <-->|Model API| Qwen

    Policy -->|通知・TTS・Intent| HAC
    Policy -->|端末操作| DroidMCP
    Policy -->|家電操作| HA
```

## スマホ側

### 役割

GalaxyはLLM本体を常時稼働させる場所ではなく、
**JARVISのセンサーハブ兼I/O端末**として扱う。

```mermaid
flowchart LR
    Sensors["位置 / バッテリー / Wi-Fi / Bluetooth / 活動状態"]
    Notifications["通知"]
    UI["Android UI"]
    Shell["Shell / Termux"]
    HACompanion["HA Companion"]
    MCP["Android MCP"]
    Shizuku["Shizuku"]
    Server["JARVIS Server"]

    Sensors --> HACompanion
    Notifications --> HACompanion
    HACompanion --> Server

    UI --> MCP
    Shell --> MCP
    Shizuku --> MCP
    MCP <--> Server
```

### 推奨コンポーネント

| コンポーネント | 役割 | 必須度 |
| --- | --- | --- |
| Home Assistant Companion | センサー・通知・位置の収集 | 高 |
| Termux | スクリプト、SSH、CLI処理 | 高 |
| Shizuku | rootなしで強いAndroid操作権限を提供 | 高 |
| droid-mcp系 | Android機能をMCP Toolとして公開 | 高 |
| Zafiro | スマホ側AI UI・直接操作 | 任意 |
| Tasker / MacroDroid | Androidイベントの補助自動化 | 任意 |

## サーバー側

### 推奨構成

```mermaid
flowchart TD
    Event["イベント受信"]
    Normalize["Event Normalizer"]
    Context["Context Builder"]
    Memory["長期記憶"]
    LLM["LLM Router"]
    Decision["Decision / Policy"]
    Ignore["無視"]
    Remember["記憶のみ"]
    Digest["Daily Briefへ追加"]
    Notify["即時通知"]
    Approve["ユーザー承認"]
    Execute["自動実行"]
    Audit["監査ログ"]

    Event --> Normalize
    Normalize --> Context
    Context <--> Memory
    Context --> LLM
    LLM --> Decision

    Decision --> Ignore
    Decision --> Remember
    Decision --> Digest
    Decision --> Notify
    Decision --> Approve
    Decision --> Execute

    Approve -->|承認| Execute
    Approve -->|拒否| Audit
    Execute --> Audit
    Notify --> Audit
    Remember --> Audit
```

### 主なサービス

| レイヤー | 推奨候補 | 目的 |
| --- | --- | --- |
| Agent Runtime | OpenClaw | 常駐エージェント、Tool実行 |
| Memory | Letta / PostgreSQL / SQLite | 長期記憶 |
| Scheduler | OpenClaw heartbeat / cron | 定期処理 |
| Smart Home | Home Assistant | IoT・スマホ状態集約 |
| Android Bridge | droid-mcp + Termux + Shizuku | Android操作 |
| Integration | MCP | Tool接続 |
| LLM Router | OpenRouter等 | モデル切替 |
| Secrets | Vault系 / OS Keyring | APIキー管理 |
| Audit | DB / append-only log | 行動記録 |

## イベント駆動

JARVISは常にLLMを回し続けるのではなく、
**意味のあるイベントが発生したときだけ起動する**。

```mermaid
sequenceDiagram
    participant Phone as Galaxy
    participant HA as Home Assistant
    participant J as JARVIS
    participant M as Memory
    participant L as LLM
    participant U as User

    Phone->>HA: Wi-Fi切断 + 位置変化
    HA->>J: Event
    J->>M: 過去の予定・行動文脈取得
    M-->>J: Context
    J->>L: 状況判定
    L-->>J: "予定先への通常移動"
    J-->>J: Intervention = none
    Note over J,U: 通知しない
```

### 重要イベントの例

```mermaid
sequenceDiagram
    participant Gmail
    participant J as JARVIS
    participant L as LLM
    participant Phone as Galaxy
    participant U as User

    Gmail->>J: 新着メール
    J->>L: 重要度・期限・必要行動を分類
    L-->>J: 今日中に回答必要
    J->>Phone: 高優先通知
    Phone->>U: 「今日中に返信が必要」
    U->>J: 返信案を作成
    J->>L: 草案生成
    L-->>J: Draft
    J->>U: 承認要求
```

## 介入ポリシー

通知疲れを防ぐため、AIの判断結果を最低でも次の5段階に分離する。

| Level | 処理 | 例 |
| --- | --- | --- |
| 0 | 無視 | 通常の位置移動 |
| 1 | 記憶のみ | 日常的な利用傾向 |
| 2 | Daily Brief | 急がない予定・支出 |
| 3 | 即時通知 | 今日中の期限、交通障害 |
| 4 | 承認要求 | メール送信、購入、設定変更 |
| 5 | 自動実行 | 明示的に事前許可した低リスク処理 |

## セキュリティ境界

AIエージェントへ最初から全権限を与えない。

```mermaid
flowchart TB
    subgraph T0["Trust Zone 0: LLM"]
        Model["外部LLM API"]
    end

    subgraph T1["Trust Zone 1: Agent"]
        Agent["OpenClaw"]
        Policy["Policy Engine"]
    end

    subgraph T2["Trust Zone 2: Tool Gateway"]
        MCP["MCP Router"]
        Allow["Allowlist"]
        Approval["Approval Gate"]
    end

    subgraph T3["Trust Zone 3: Devices / Accounts"]
        Phone["Android"]
        HA["Home Assistant"]
        Mail["Gmail"]
        Finance["金融・決済"]
    end

    Model <--> Agent
    Agent --> Policy
    Policy --> MCP
    MCP --> Allow
    Allow --> Approval

    Approval -->|低リスク| Phone
    Approval -->|低リスク| HA
    Approval -->|要承認| Mail
    Approval -->|原則要承認| Finance
```

### 原則

- LLMへAPIキーそのものを見せない。
- MCPサーバーごとに権限を分離する。
- 読み取り権限と書き込み権限を分ける。
- Android shellはallowlist方式にする。
- Shizuku経由の危険なコマンドを直接LLMへ公開しない。
- メール送信、購入、送金、削除は原則承認制にする。
- すべての自動操作を監査ログへ残す。
- 秘密情報をMemoryへ保存しない。
- 外部からAndroid MCPへ直接到達させない。

## ネットワーク構成

```mermaid
flowchart LR
    Internet["Internet"]
    VPN["Tailscale / WireGuard"]

    subgraph VPS["OCI / VPS"]
        JARVIS["JARVIS Server"]
    end

    subgraph Home["Home LAN"]
        HA["Home Assistant"]
        Devices["IoT Devices"]
    end

    subgraph Mobile["Galaxy"]
        AndroidMCP["Android MCP"]
    end

    Internet --> JARVIS
    JARVIS <--> VPN
    VPN <--> HA
    VPN <--> AndroidMCP
    HA <--> Devices
```

外部公開するのは原則JARVIS Serverのみとし、
Home AssistantとAndroid MCPはVPN内へ閉じる。

## 日常利用イメージ

```mermaid
flowchart TD
    Morning["07:30<br/>スマホを充電器から外す"]
    Wake["JARVIS起動"]
    Collect["Calendar / Gmail / 天気 / スマホ状態"]
    Judge["重要事項を判定"]
    Brief["朝のBrief"]
    Day["日中イベント監視"]
    Important{"重要?"}
    Silent["何もしない"]
    Alert["通知"]
    Action{"自動処理可能?"}
    Auto["自動処理"]
    Confirm["承認要求"]

    Morning --> Wake
    Wake --> Collect
    Collect --> Judge
    Judge --> Brief
    Brief --> Day
    Day --> Important
    Important -->|No| Silent
    Important -->|Yes| Alert
    Alert --> Action
    Action -->|Yes| Auto
    Action -->|No| Confirm
```

## 実装順

1. **Home Assistantをハブ化**
   - GalaxyへCompanion Appを導入。
   - バッテリー、ネットワーク、通知など必要なセンサーだけ有効化。
   - SwitchBot等をHome Assistantへ統合。

2. **GalaxyをTool Provider化**
   - Termuxを構築。
   - Shizuku + rishを設定。
   - Android MCPをTermux側へ接続。
   - 読み取り系Toolから公開する。

3. **JARVIS Serverを構築**
   - OCI / VPSへ常駐Agent Runtimeを配置。
   - MCP Clientを設定。
   - Home AssistantとAndroid MCPへVPN経由で接続。

4. **LLMを分離**
   - Agent Runtimeから直接特定モデルへ固定しない。
   - OpenAI、Claude、Gemini、Qwen等を切り替え可能にする。

5. **Memory追加**
   - 最初はSQLite/PostgreSQLで十分。
   - 長期人物モデルが必要になった段階でLetta等を追加。

6. **Policy Engine追加**
   - read-only
   - notify
   - approval-required
   - pre-approved automation

   の4種類程度から開始する。

7. **自動化を段階的に増やす**
   - 最初は観測と通知だけ。
   - 次にHome Assistant操作。
   - 最後にAndroid UIや外部サービスへの書き込みを許可する。

## 最小構成

最初から全部導入する必要はない。

```mermaid
flowchart LR
    Phone["Galaxy<br/>HA Companion"]
    HA["Home Assistant"]
    Server["OpenClaw<br/>JARVIS Server"]
    LLM["LLM API"]

    Phone --> HA
    HA <--> Server
    Server <--> LLM
    Server --> Phone
```

この段階だけでも、

- 在宅・外出判定
- バッテリー状態
- Wi-Fi状態
- スマホ通知
- Calendar
- Gmail
- 天気
- Home Assistantイベント

を使った状況認識型エージェントを作れる。

その後、

```text
Termux
  ↓
Shizuku
  ↓
Android MCP
  ↓
JARVIS
```

を追加して、
**「観測するJARVIS」から「実際に操作するJARVIS」へ拡張する**
のが安全かつ実装しやすい。
