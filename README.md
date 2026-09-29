# Discord AI Discussion Bot

Rust製のDiscord議論Botです。`/talk` で質問し、同じチャンネルの過去の投稿を文脈としてOllama Cloudの `gpt-oss:120b` に渡します。BotとMariaDB 12.3をDocker Composeで起動できます。GPU・ローカルOllamaは不要です。

本番（Oracle Cloud の 1GB VM）の構築・デプロイ・バックアップ・監視は [docs/runbook.md](docs/runbook.md) を参照してください。本番ではイメージをビルドせず、GitHub Actions がビルドして GHCR に置いたイメージを digest 指定で使います。

## 使い方

```text
/talk message:この議論の論点を整理して
/talk message:最新情報も調べて比較して web_search:true history:2h
/talk message:この設計を評価して history:0m
```

| オプション | デフォルト | 内容 |
| --- | --- | --- |
| `message` | 必須 | 質問本文。1〜4000文字 |
| `web_search` | `false` | `true` の呼び出しだけWeb検索・ページ取得を許可 |
| `history` | `15m` | `30m`、`2h` など整数の分・時間。最大24時間。`0m` / `0h` で履歴なし |

オプションは毎回独立しています。前の呼び出しの設定は引き継ぎません。通常のテキストチャンネル・既存スレッド・フォーラム投稿内で利用できます。回答は公開されます。DM、画像・添付ファイル解析、メンションによる呼び出しは実装していません。

**利用制御:** Bot自体はロールによる利用制限を行いません。誰が `/talk` を使えるかは、サーバーごとにDiscordのサーバー設定内の連携サービス（Integrations）のコマンド権限で制御してください。設定はサーバー単位で完結し、Bot側の再起動や設定変更は不要です。未設定の場合、既定でそのサーバーの全員が利用できます。利用を制限した相手が呼び出すと、Discordが「アプリケーションの応答がありません」（クライアント言語等によって表記が異なる場合あり）を表示します。サーバーの通常投稿は、発言者に関わらず履歴の参照対象です。

## セットアップ

### 1. Discordアプリ

1. [Discord Developer Portal](https://discord.com/developers/applications) でアプリを作成し、Botトークンを取得します。
2. **Bot → Privileged Gateway Intents → Message Content Intent** を有効にします。Server Members Intentは不要です。大規模な導入でIntentの審査が必要になった場合はDiscordの案内に従ってください。
3. **Installation / OAuth2 URL Generator** でサーバーへのインストールを選び、`bot` と `applications.commands` のスコープを設定します。本番では Install Link を None にして **Public Bot を OFF** にし、運営者が許可したサーバーにだけ追加します（手順は [docs/runbook.md](docs/runbook.md) の「サーバーを追加する」）。
4. Botに `View Channels`、`Read Message History`、`Send Messages`、`Send Messages in Threads` の権限を付けて招待します。Administratorは不要です。プライベートスレッドではBotもメンバーとして参加させます。
5. 利用者を制限したいサーバーでは、サーバー設定の連携サービス（Integrations）で `/talk` を使えるロール・ユーザーを設定します。サーバーごとに独立した設定です。

履歴参照時はBotと実行者の両方に閲覧・履歴閲覧権限が必要です。Botが読めないチャンネルへ権限を拡大する動作はありません。

### 2. `.env`

PowerShellで、まだ `.env` がなければコピーします。

```powershell
Copy-Item .env.example .env
```

以下を編集します。秘密情報に `$` や `#` が含まれる場合は値をシングルクォートで囲んでください。

```dotenv
BOT_IMAGE=ghcr.io/mugicomugi/ai-chat-for-discord@sha256:replace_with_digest_from_ci
DISCORD_TOKEN=your_discord_bot_token
OLLAMA_API_KEY=your_ollama_api_key
OLLAMA_MODEL=gpt-oss:120b
MARIADB_DATABASE=discussion
MARIADB_USER=discussion
MARIADB_PASSWORD=your_long_random_password
MARIADB_ROOT_PASSWORD=your_different_long_random_root_password
```

- `BOT_IMAGE` は GitHub Actions（`ci` ワークフロー）の実行結果の Summary に表示される digest 付きのイメージです。本番では `scripts/deploy.sh` が書き換えます。手元でビルドする場合は `discord-discussion-bot:local` など任意の名前にします。
- Ollamaキーは [Ollamaの設定](https://ollama.com/settings/keys) で発行します。推論と検索で同じキーを使用します。
- `/talk` はグローバルコマンドとして登録され、Botをインストールした全サーバーで利用できます。登録直後は反映まで最大1時間ほどかかる場合があります。会話履歴・DBの記録はサーバー（ギルド）ごとに分離されます。
- `RETENTION_DAYS=30`：DBの保持期間（1〜3650日）。起動時と1時間ごとに期限切れを削除します。
- `REQUEST_TIMEOUT_SECONDS=180`：履歴取得から回答投稿までのタイムアウト（10〜600秒）。個々のOllama HTTPリクエストは最大120秒です。
- `RUST_LOG=discord_discussion_bot=info`：通常ログ。本文・キー・DBパスワード・内部推論は記録しません。依存ライブラリの詳細ログを有効にする場合は、そこに含まれるデータに注意してください。

`.env` はGitとDockerのビルドコンテキストから除外しています。Composeは各サービスに必要な変数だけを渡します。DBのrootパスワードはBotへ渡しません。

### 3. 起動

本番の VM では [docs/runbook.md](docs/runbook.md) の手順で `scripts/deploy.sh` を使います。手元（Docker Desktop を Linux コンテナモードで起動）で試す場合は、`compose.build.yaml` を重ねてローカルでビルドします。

```powershell
$env:BOT_IMAGE = 'discord-discussion-bot:local'
docker compose -f compose.yaml -f compose.build.yaml up -d --build
docker compose ps
docker compose logs --tail 100 bot
```

ログに `database_ready` と `discord_ready` が出たら `/talk` を実行します。MariaDBはヘルスチェック完了後にBotへ接続され、スキーマは自動作成されます。DBは外部に出られない内部ネットワークに置き、ポートはホストへ公開しません。Botは非root・読み取り専用ファイルシステムで、メモリ上限（256MB）付きで起動します。

停止・再起動:

```powershell
docker compose stop
docker compose up -d
```

設定変更後は `docker compose up -d` で反映します。

## 履歴・検索・保存の仕様

- 参照期間は `/talk` の呼び出し日時を基準とし、開始時刻を含み、呼び出し時刻以降の投稿は含みません。スレッドと親チャンネルは独立しています。
- 通常投稿は必要時にDiscordから取得し、常時収集・DB保存はしません。対象は人間のテキスト投稿と本Botの回答です。他Botと外部Webhookは除外します。
- 過去の `/talk` の質問はMariaDBから取得します。Bot回答はDiscordに現存するメッセージを使うため、再起動しても参照できます。削除されたBot回答をDBから復活させることはありません。質問はDBの保持期間中は残ります。
- 時系列に整列し、ID重複を除き、最新500件・本文合計60,000 Unicode文字までAIに渡します。Discord取得は1回100件、最大10ページです。混雑したチャンネルなどで上限に達したら省略を明示します。発言者と日時も渡します。
- `history:0m` は通常投稿・過去の質問の両方を参照しません。現在の質問・回答は保存します。
- `web_search:false` ではツール定義をモデルに渡さず、モデルがツール呼び出しを返しても実行しません。
- `web_search:true` ではAIが必要に応じてOllamaの検索・ページ取得APIを呼びます。最大5ツール実行、検索1回5件、検索本文1件4000文字・取得ページ8000文字を上限とします。取得した出典URL一覧を回答末尾に付けます。検索が不要と判断された場合は実行しません。
- AIに渡す会話、検索語、ページURLはOllama Cloudへ送信されます。ツールにはWeb検索・ページ取得だけを公開し、DB・シェル・Discord操作は公開しません。
- DBには質問、回答、出典URL・タイトル、ID、オプション、処理状態、返信IDを保存します。通常投稿、取得ページ本文、内部推論、Interactionトークンは保存しません。
- 同じチャンネルでは1件のみ、Bot全体で最大4件を処理します。満杯なら即座に再試行を案内します。
- 再起動時の処理中レコードは失敗に変更し、自動再生成・再投稿しません。Discord投稿とDB更新は単一トランザクションにできないため、投稿直後の障害では返信IDが保存されないことがあります。重複投稿を避けるため、自動再送は行いません。
- Botは**1プロセス／1レプリカ**で運用してください。複数レプリカでの実行制御・起動時復旧には対応していません。

## 開発とテスト

Rust 1.98以上を使用します。依存関係は `Cargo.lock` で固定しています。GitHub Actions（`.github/workflows/ci.yml`）が push と pull request のたびに整形・Clippy・テスト・実DBテスト（MariaDB 12.3.3）を実行し、main へのマージ時にイメージを GHCR へ push します。

```powershell
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
docker build -t discord-discussion-bot:local .
```

Docker内で整形チェック・Clippy・単体／モックAPIテストを実行できます。

```powershell
docker build --target test -t discord-discussion-bot:test .
```

実DBテストは本番と別のComposeプロジェクト・ボリュームを使います。`discussion_test` 以外のDB名ではテストを拒否します。

```powershell
docker compose -f compose.test.yaml -p discussion-bot-test up -d --wait
$env:TEST_DATABASE_URL = 'mysql://test:test_only_password@127.0.0.1:33316/discussion_test'
cargo test --locked --test database database_lifecycle -- --ignored --exact
docker compose -f compose.test.yaml -p discussion-bot-test restart db-test
docker compose -f compose.test.yaml -p discussion-bot-test up -d --wait
cargo test --locked --test database persistence_after_restart -- --ignored --exact
Remove-Item Env:TEST_DATABASE_URL
```

ホストのRust環境を使わない場合は、上記の各 `cargo test` を次の形式で実行できます。

```powershell
docker run --rm --network discussion-bot-test_default -e TEST_DATABASE_URL=mysql://test:test_only_password@db-test:3306/discussion_test discord-discussion-bot:test --test database database_lifecycle -- --ignored --exact
```

再起動後はテスト名を `persistence_after_restart` に変更します。検証用データを破棄する場合だけ、`docker compose -f compose.test.yaml -p discussion-bot-test down -v` を実行してください。

実際のOllama接続・検索を確認するテストも用意しています。`.env` のキーを使い、固定のテスト質問だけを送信してAPI利用枠を消費します。通常の `cargo test` では実行されません。

```powershell
cargo test --locked --test live_ollama -- --ignored
```

### 実サービスの確認

実トークンと利用ロールの設定後、以下をテストサーバーで確認します。

1. 既定の `/talk` が応答する。サーバー設定の連携サービスで利用を制限したユーザーは無応答エラーとなり、DBにレコードが増えない。
2. 過去15分内の通常投稿が反映され、`history:2h` では範囲が広がり、`history:0m` では参照されない。
3. `web_search:true` で検索を明示的に依頼すると出典付きで回答する。省略時・`false` では検索しない。
4. 別チャンネル・親チャンネルの会話がスレッド内の文脈へ混入しない。
5. 同じチャンネルでの連続呼び出し、長文、権限不足、再起動後の会話継続を確認する。

## 更新・バックアップ

本番の VM では、毎日の暗号化バックアップ（`scripts/backup.sh`、systemd タイマー）とデプロイ前のバックアップが自動で取られます。更新・切り戻し・復元の手順は [docs/runbook.md](docs/runbook.md) を参照してください。SQLマイグレーションは追加だけの前方向で、古いイメージも新しいスキーマで起動できます。

手元の環境では、次のコマンドでバックアップを取れます（`--hex-blob` はバイナリ列を安全に出力するためのものです）。

```powershell
New-Item -ItemType Directory -Force backups
docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb-dump -uroot --single-transaction --hex-blob "$MARIADB_DATABASE" > /tmp/discussion.sql'
docker compose cp db:/tmp/discussion.sql ./backups/discussion.sql
docker compose exec db rm /tmp/discussion.sql
docker compose -f compose.yaml -f compose.build.yaml up -d --build
```

復元はBotを停止し、復元先のDBを確認してから実施します。次の操作は現在のDB内容をバックアップ時点へ戻します。

```powershell
docker compose stop bot
docker compose cp ./backups/discussion.sql db:/tmp/discussion.sql
docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot "$MARIADB_DATABASE" < /tmp/discussion.sql'
docker compose exec db rm /tmp/discussion.sql
docker compose up -d bot
```

バックアップには会話が含まれるため、アクセスを制限して保管してください。本番の `docker compose down -v` はDBボリュームを削除するので、通常の停止では使いません。DBパスワードを `.env` だけで変更しても、既存ボリューム内のDBユーザーのパスワードは変更されません。

## 参照仕様

- [Ollama Cloud直接接続とモデル名](https://docs.ollama.com/cloud)
- [Ollama Chat API](https://docs.ollama.com/api/chat)
- [Ollama Web検索・ページ取得](https://docs.ollama.com/capabilities/web-search)
- [Discord Gateway / Message Content Intent](https://docs.discord.com/developers/events/gateway)
