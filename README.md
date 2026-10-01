# Discord AI Discussion Bot

Rust製のDiscord議論Botです。`/talk` で質問し、同じチャンネルの過去の投稿を文脈としてOllama Cloudの `gpt-oss:120b` に渡します。BotとMariaDB 12.3をDocker Composeで起動できます。GPU・ローカルOllamaは不要です。

本番（Oracle Cloud の 1GB VM）の構築・デプロイ・バックアップ・監視は [docs/runbook.md](docs/runbook.md) を参照してください。本番のイメージは VM 上で `scripts/build-image.sh` によりビルドします（メモリ上限と低い優先度で動かし、稼働中の Bot と DB への影響を抑えます）。

## 使い方

```text
/talk message:この議論の論点を整理して
/talk message:最新情報も調べて比較して web_search:true history:2h
/talk message:この設計を評価して history:0m
/talk message:今週の議論をまとめて history:7d
```

| オプション | デフォルト | 内容 |
| --- | --- | --- |
| `message` | 必須 | 質問本文。1〜4000文字 |
| `web_search` | `false` | `true` の呼び出しだけWeb検索・ページ取得を許可 |
| `history` | `15m` | `30m`、`2h`、`3d` など整数の分・時間・日。最大7日（`7d` / `168h` / `10080m`）で、その中の直近100件まで。`0m` / `0h` / `0d` で履歴なし |

オプションは毎回独立しています。前の呼び出しの設定は引き継ぎません。通常のテキストチャンネル・既存スレッド・フォーラム投稿内で利用できます。回答は公開されます。DM、画像・添付ファイル解析、メンションによる呼び出しは実装していません。

**利用制御:** 次の2つを両方満たす人だけが `/talk` を使えます。サーバーの管理者も例外ではありません。

1. そのサーバーが運営者の許可リストに入っている（`ops guild allow`、[docs/runbook.md](docs/runbook.md) の「サーバーを追加する」）。
2. そのサーバーで「利用ロール」に設定されたロールを持っている。**利用ロールが未設定のサーバーでは誰も使えません。** 全員に許可する場合は `@everyone` を利用ロールにします。

利用ロールは、各サーバーで「サーバー管理」権限を持つ人が `/config` か [Web管理画面](#web管理画面) で設定します（`/config` の応答は本人にだけ表示されます）。

```text
/config role-add role:@メンバー
/config role-add role:@運営 type:ナレッジ管理
/config role-remove role:@メンバー
/config show
```

- `/config show` は、Web管理画面を有効にしている場合、そのサーバーの設定画面のURLも表示します。
- 「ナレッジ管理」ロールは、今後追加するナレッジベースの資料を管理できるロールです（[docs/roadmap.md](docs/roadmap.md)）。ロールの設定を変更できるのは、サーバーのオーナーと「管理者」「サーバー管理」権限を持つ人だけです。
- Discordで削除したロールは、設定からも自動で外れます。
- 利用できない人が `/talk` を実行すると、理由が本人にだけ表示されます。履歴の取得・DBへの保存・AIの呼び出しは行いません。
- Discordのサーバー設定の連携サービス（Integrations）でコマンド権限を設定すると、`/talk` を表示する相手をさらに絞れます。
- サーバーの通常投稿は、発言者が利用ロールを持つかどうかに関わらず履歴の参照対象です。

## セットアップ

### 1. Discordアプリ

1. [Discord Developer Portal](https://discord.com/developers/applications) でアプリを作成し、Botトークンを取得します。
2. **Bot → Privileged Gateway Intents → Message Content Intent** を有効にします。Server Members Intentは不要です。大規模な導入でIntentの審査が必要になった場合はDiscordの案内に従ってください。
3. **Installation / OAuth2 URL Generator** でサーバーへのインストールを選び、`bot` と `applications.commands` のスコープを設定します。本番では Install Link を None にして **Public Bot を OFF** にし、運営者が許可したサーバーにだけ追加します（手順は [docs/runbook.md](docs/runbook.md) の「サーバーを追加する」）。
4. Botに `View Channels`、`Read Message History`、`Send Messages`、`Send Messages in Threads` の権限を付けて招待します。Administratorは不要です。プライベートスレッドではBotもメンバーとして参加させます。
5. 運営者がサーバーを許可リストに追加し（`ops guild allow`）、そのサーバーの管理者が `/config role-add` で利用ロールを設定します。

履歴参照時はBotと実行者の両方に閲覧・履歴閲覧権限が必要です。Botが読めないチャンネルへ権限を拡大する動作はありません。

### 2. `.env`

PowerShellで、まだ `.env` がなければコピーします。

```powershell
Copy-Item .env.example .env
```

以下を編集します。秘密情報に `$` や `#` が含まれる場合は値をシングルクォートで囲んでください。

```dotenv
BOT_IMAGE=discord-discussion-bot:git-replace_with_commit
DISCORD_TOKEN=your_discord_bot_token
OLLAMA_API_KEY=your_ollama_api_key
OLLAMA_MODEL=gpt-oss:120b
MARIADB_DATABASE=discussion
MARIADB_USER=discussion
MARIADB_PASSWORD=your_long_random_password
MARIADB_ROOT_PASSWORD=your_different_long_random_root_password
```

- `BOT_IMAGE` は起動するイメージです。本番では `scripts/build-image.sh` が表示するタグ（`discord-discussion-bot:git-<コミット>`）を `scripts/deploy.sh` に渡すと書き換わります。手元でビルドする場合は `discord-discussion-bot:local` など任意の名前にします。
- Ollamaキーは [Ollamaの設定](https://ollama.com/settings/keys) で発行します。推論と検索で同じキーを使用します。
- `/talk` と `/config` はグローバルコマンドとして登録され、Botをインストールした全サーバーに表示されます（使えるのは許可リストに入ったサーバーだけです）。登録直後は反映まで最大1時間ほどかかる場合があります。会話履歴・DBの記録はサーバー（ギルド）ごとに分離されます。
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

## Web管理画面

Discordアカウントでログインして、Botを導入しているサーバーでの自分の権限を確認し、サーバー管理者は利用ロール・ナレッジ管理ロールを設定できる画面です（今後ナレッジ管理とWebチャットを追加します。[docs/roadmap.md](docs/roadmap.md)）。BotとMariaDBに加えてCaddy（HTTPS）を動かし、`https://<ドメイン>/` で公開します。**任意の機能**で、下の3つの環境変数を設定しなければ起動せず、Botはこれまでどおり動きます。本番での公開手順（DNS、OCIのポート、Caddy、監視）は [docs/runbook.md](docs/runbook.md) の「12. Web 管理画面を公開する」を参照してください。

### 設定

1. [Discord Developer Portal](https://discord.com/developers/applications) のアプリの **OAuth2** で次を行います。
   - **Redirects** に `https://<ドメイン>/auth/callback` を追加して保存する（1文字でも違うとログインできません）。
   - **Client ID** を控え、**Client Secret** を発行（Reset Secret）して控える。Client Secret はBotトークンと同じく秘密情報です。
2. `.env` に追加します。

   ```dotenv
   DISCORD_CLIENT_ID=123456789012345678
   DISCORD_CLIENT_SECRET=your_client_secret
   PUBLIC_BASE_URL=https://bot.example.com
   DOMAIN=bot.example.com
   COMPOSE_PROFILES=web
   ```

3. `docker compose up -d` で、Botの再作成とCaddyの起動を行います。

| 変数 | 既定 | 内容 |
| --- | --- | --- |
| `DISCORD_CLIENT_ID` / `DISCORD_CLIENT_SECRET` | なし | OAuth2 の Client ID と Client Secret |
| `PUBLIC_BASE_URL` | なし | 画面のURL（`https://<ドメイン>`、パスなし）。`https` のみ。手元の確認用に `http://localhost[:ポート]` だけ例外 |
| `WEB_BIND` | `0.0.0.0:8080` | Botが待ち受けるアドレス。Composeでは `0.0.0.0:8080` に固定（ホストには公開せず、Caddyだけが接続）。`cargo run` で手元から使うときだけ設定します |
| `DOMAIN` | なし | Caddyが証明書を取得するドメイン（`PUBLIC_BASE_URL` のホスト名と同じ） |
| `COMPOSE_PROFILES` | なし | `web` でCaddyを起動します |

`DISCORD_CLIENT_ID`・`DISCORD_CLIENT_SECRET`・`PUBLIC_BASE_URL` は3つそろうと有効になり、一部だけ設定するとBotは起動エラーになります。

### 仕様

- **ログイン**：Discordの認可画面（スコープ `identify guilds`）で、ユーザーIDと名前、参加しているサーバーの一覧だけを読み取ります。Discordのアクセストークンは読み取り後すぐに無効化（revoke）し、保存しません。
- **セッション**：ログインのたびに新しいランダムなトークンを発行し、Cookie（`__Host-session`、Secure・HttpOnly・SameSite=Lax）に入れます。DBにはトークンのSHA-256だけを保存します。有効期限はログインから7日で、使っても延長しません。1人10件まで（超えると古いものから削除）。期限切れのセッションは1時間ごとに削除します。
- **表示するサーバー**：ログイン時に参加していて、運営者の許可リストに入っていて、Botが参加しているサーバーだけです。一覧はログイン時点のものなので、新しく参加したサーバーはログインし直すと表示されます。
- **権限**：`/talk` と同じ判定です。ロール設定を変更できるのはオーナーと「管理者」「サーバー管理」権限を持つ人だけで、Botの利用には利用ロールが必要です。メンバーのロールと権限はBotトークンでDiscordから取得し、60秒（サーバーのロールの定義とオーナーは5分。Discordでロールが作成・変更・削除されると即破棄）キャッシュします。ロールを外された人の画面上の権限は最大60秒残ります。ロールの設定自体はDBから毎回読むので、保存すると `/talk` にもすぐ反映されます。
- **ロール設定の保存**：利用ロールとナレッジ管理ロールを1回の操作でまとめて置き換えます（各25個まで。サーバーに存在しないロールは保存できません）。Discordで削除済みのロールが設定に残っていた場合は、保存すると外れます。
- **安全対策**：インラインスクリプトを使わず、外部のスクリプト・画像を読み込まない Content-Security-Policy、`X-Content-Type-Options: nosniff` などのヘッダーを付けます。GET以外のリクエストは `Origin` が `PUBLIC_BASE_URL` と一致しなければ拒否し、JSONの送信には `Content-Type: application/json` を必須にしています（CSRF対策）。HTTPS・HSTSはCaddyが担当します。
- **ページ**：`/privacy` と `/terms` で [docs/privacy.md](docs/privacy.md) と [docs/terms.md](docs/terms.md) を表示します（イメージに組み込むので、変更はイメージの再ビルドで反映されます）。`/healthz` は外部監視用です（DBとDiscordへの接続が正常なら200）。
- 画面はビルド不要の素のHTML/JS/CSS（`static/`）で、バイナリに組み込まれます。将来のMarkdown表示用に [marked](https://github.com/markedjs/marked) と [DOMPurify](https://github.com/cure53/DOMPurify) を `static/vendor/` に同梱しています（ライセンスは `static/vendor/LICENSES.txt`）。

### 手元で試す

Developer Portal の Redirects に `http://localhost:8080/auth/callback` も追加し、`.env` で `PUBLIC_BASE_URL=http://localhost:8080` と `WEB_BIND=127.0.0.1:8080` を設定して `cargo run` します（このときCookieは `__Host-` なし・Secureなしになります）。

## 履歴・検索・保存の仕様

- 参照期間は `/talk` の呼び出し日時を基準とし、開始時刻を含み、呼び出し時刻以降の投稿は含みません。スレッドと親チャンネルは独立しています。
- 通常投稿は必要時にDiscordから取得し、常時収集・DB保存はしません。対象は人間のテキスト投稿と本Botの回答です。他Botと外部Webhookは除外します。
- 過去の `/talk` の質問はMariaDBから取得します。Bot回答はDiscordに現存するメッセージを使うため、再起動しても参照できます。削除されたBot回答をDBから復活させることはありません。質問はDBの保持期間中は残ります。
- 時系列に整列し、ID重複を除き、最新100件・本文合計60,000 Unicode文字までAIに渡します。Discord取得は1回100件、最大10ページです。混雑したチャンネルなどで上限に達したら省略を明示します。発言者と日時も渡します。
- `history:0m` は通常投稿・過去の質問の両方を参照しません。現在の質問・回答は保存します。
- `web_search:false` ではツール定義をモデルに渡さず、モデルがツール呼び出しを返しても実行しません。
- `web_search:true` ではAIが必要に応じてOllamaの検索・ページ取得APIを呼びます。最大5ツール実行、検索1回5件、検索本文1件4000文字・取得ページ8000文字を上限とします。取得した出典URL一覧を回答末尾に付けます。検索が不要と判断された場合は実行しません。
- AIに渡す会話、検索語、ページURLはOllama Cloudへ送信されます。ツールにはWeb検索・ページ取得だけを公開し、DB・シェル・Discord操作は公開しません。
- DBには質問、回答、出典URL・タイトル、ID、オプション、処理状態、返信IDを保存します。通常投稿、取得ページ本文、内部推論、Interactionトークンは保存しません。
- 同じチャンネルでは1件のみ、Bot全体で最大4件を処理します。満杯なら即座に再試行を案内します。
- 再起動時の処理中レコードは失敗に変更し、自動再生成・再投稿しません。Discord投稿とDB更新は単一トランザクションにできないため、投稿直後の障害では返信IDが保存されないことがあります。重複投稿を避けるため、自動再送は行いません。
- Botは**1プロセス／1レプリカ**で運用してください。複数レプリカでの実行制御・起動時復旧には対応していません。

## 開発とテスト

Rust 1.98以上を使用します。依存関係は `Cargo.lock` で固定しています。本番の VM でテストするときは、`scripts/cargo-vm.sh`（メモリ上限と低い優先度で cargo を実行）を使います。例: `scripts/cargo-vm.sh clippy --locked --all-targets -- -D warnings`。GitHub Actions の設定（`.github/workflows/ci.yml`）もあり、Actions が使える状態なら push と pull request のたびに同じ検査を実行します。

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
cargo test --locked --test database access_and_guilds -- --ignored --exact
cargo test --locked --test web web_sessions -- --ignored --exact
cargo test --locked --test web web_login_keeps_only_allowlisted_guilds -- --ignored --exact
cargo test --locked --test web web_role_settings -- --ignored --exact
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

実トークンを設定し、テストサーバーを許可リストに入れてから、以下を確認します。

1. 許可リストにないサーバー、利用ロールを持たないユーザー（管理者を含む）では、`/talk` が理由を本人にだけ表示して終わり、DBにレコードが増えない。
2. `/config role-add` で利用ロールを付けると `/talk` が応答し、`/config role-remove` で外すと再び使えなくなる。「サーバー管理」権限のないユーザーは `/config` を変更できない。
3. 過去15分内の通常投稿が反映され、`history:2h` や `history:3d` では範囲が広がり、`history:0m` では参照されない。`history:8d` は入力エラーになる。
4. `web_search:true` で検索を明示的に依頼すると出典付きで回答する。省略時・`false` では検索しない。
5. 別チャンネル・親チャンネルの会話がスレッド内の文脈へ混入しない。
6. 同じチャンネルでの連続呼び出し、長文、権限不足、再起動後の会話継続を確認する。
7. Web管理画面を有効にした場合：ログイン後のCookieが `__Host-session`（Secure・HttpOnly・SameSite=Lax）であること、ブラウザーの開発者ツールのコンソールにCSP違反が出ないこと、「サーバー管理」権限のないアカウントにはロール設定が表示されないこと、Web画面で保存したロールで `/talk` が使えること、Discordでロールを外すと60秒以内にWeb画面の権限からも外れること、ログアウト後に `/api/me` が401になること。

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
