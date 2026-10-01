# Discord AI Discussion Bot

Rust製のDiscord議論Botです。`/talk` で質問し、同じチャンネルの過去の投稿を文脈としてOllama Cloudの `gpt-oss:120b` に渡します。Web画面（任意）では、同じBotと会話できる[Webチャット](#webチャット)も使えます。BotとMariaDB 12.3をDocker Composeで起動できます。GPU・ローカルOllamaは不要です。

本番（Oracle Cloud の 1GB VM）の構築・デプロイ・バックアップ・監視は [docs/runbook.md](docs/runbook.md) を参照してください。本番のイメージは VM 上で `scripts/build-image.sh` によりビルドします（メモリ上限と低い優先度で動かし、稼働中の Bot と DB への影響を抑えます）。

## 使い方

```text
/talk message:この議論の論点を整理して
/talk message:最新情報も調べて比較して web_search:true history:2h
/talk message:この設計を評価して history:0m
/talk message:今週の議論をまとめて history:7d
/talk message:社内手順書に沿って答えて knowledge:true
```

| オプション | デフォルト | 内容 |
| --- | --- | --- |
| `message` | 必須 | 質問本文。1〜4000文字 |
| `web_search` | `false` | `true` の呼び出しだけWeb検索・ページ取得を許可 |
| `history` | `15m` | `30m`、`2h`、`3d` など整数の分・時間・日。最大7日（`7d` / `168h` / `10080m`）で、その中の直近100件まで。`0m` / `0h` / `0d` で履歴なし |
| `knowledge` | 資料があれば参照 | このサーバーの[ナレッジベース](#ナレッジベース)の資料を参照するか。`false` で参照しません。`true` で参照できる資料がないときは、その旨を回答に付記します |

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
- 「ナレッジ管理」ロールは、[ナレッジベース](#ナレッジベース)の資料を Web 管理画面で登録・削除できるロールです（サーバー管理権限を持つ人は設定しなくても管理できます）。ロールの設定を変更できるのは、サーバーのオーナーと「管理者」「サーバー管理」権限を持つ人だけです。
- Discordで削除したロールは、設定からも自動で外れます。
- 利用できない人が `/talk` を実行すると、理由が本人にだけ表示されます。履歴の取得・DBへの保存・AIの呼び出しは行いません。
- Discordのサーバー設定の連携サービス（Integrations）でコマンド権限を設定すると、`/talk` を表示する相手をさらに絞れます。
- サーバーの通常投稿は、発言者が利用ロールを持つかどうかに関わらず履歴の参照対象です。

**自分のデータの確認と削除:** `/privacy` は、許可リストやロールに関係なく誰でも使えます（自分のデータだけが対象で、応答は本人にだけ表示されます）。

```text
/privacy show
/privacy delete
```

- `show` は、このBotが保存している自分の `/talk` の記録・Webチャットの会話・Webのログインの件数（全サーバーの合計）を表示します。Web管理画面を有効にしている場合は、Web画面の「あなたのデータ」（`/#/privacy`）のURLも表示します。
- `delete` は確認ボタン（10分間有効、押せるのは実行した本人だけ）を押すと、`/talk` の記録（返信の記録を含む）・Webチャットの会話（メッセージを含む）・Webのログイン（ログアウトされます）を削除し、ナレッジ資料とロール設定に記録された自分のIDと名前を消します（資料と設定はサーバーのものなので残ります）。回答を作成中の `/talk` の記録と、チャンネルに投稿された回答のメッセージは残ります。Web画面の「あなたのデータ」でも同じ削除ができます。
- 削除したユーザーIDと日時は、バックアップから復元したときに削除し直すため40日間だけ記録します（[docs/privacy.md](docs/privacy.md)）。

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
- `/talk`・`/config`・`/privacy` はグローバルコマンドとして登録され、Botをインストールした全サーバーに表示されます（`/talk` と `/config` を使えるのは許可リストに入ったサーバーだけです）。登録直後は反映まで最大1時間ほどかかる場合があります。会話履歴・DBの記録はサーバー（ギルド）ごとに分離されます。
- `RETENTION_DAYS=30`：DBの保持期間（1〜3650日）。起動時と1時間ごとに期限切れを削除します。
- `GUILD_PURGE_GRACE_DAYS=14`：Botがサーバーから外されてから、そのサーバーのデータ（`/talk` の記録・Webチャットの会話・ナレッジ資料・ロール設定）を削除するまでの猶予（1〜365日、空なら14日）。猶予中に再び招待されれば、そのまま使い続けられます。削除したサーバーは許可リストからも外れるので、再び使うには `ops guild allow` が必要です（[docs/runbook.md](docs/runbook.md)「Bot がサーバーから外されたとき」）。
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

Discordアカウントでログインして、Botを導入しているサーバーでの自分の権限を確認し、利用ロールを持つ人は[Webチャット](#webチャット)でAIと会話でき、サーバー管理者は利用ロール・ナレッジ管理ロールを、ナレッジ管理ロールを持つ人は[ナレッジベース](#ナレッジベース)の資料を管理できる画面です。BotとMariaDBに加えてCaddy（HTTPS）を動かし、`https://<ドメイン>/` で公開します。**任意の機能**で、下の3つの環境変数を設定しなければ起動せず、Botはこれまでどおり動きます。本番での公開手順（DNS、OCIのポート、Caddy、監視）は [docs/runbook.md](docs/runbook.md) の「12. Web 管理画面を公開する」を参照してください。

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
| `WEB_DAILY_MESSAGES_PER_USER` | `100` | [Webチャット](#webチャット)で1人が直近24時間に送れるメッセージ数（1〜100000） |
| `COMPOSE_PROFILES` | なし | `web` でCaddyを起動します |

`DISCORD_CLIENT_ID`・`DISCORD_CLIENT_SECRET`・`PUBLIC_BASE_URL` は3つそろうと有効になり、一部だけ設定するとBotは起動エラーになります。

### 仕様

- **ログイン**：Discordの認可画面（スコープ `identify guilds`）で、ユーザーIDと名前、参加しているサーバーの一覧だけを読み取ります。Discordのアクセストークンは読み取り後すぐに無効化（revoke）し、保存しません。
- **セッション**：ログインのたびに新しいランダムなトークンを発行し、Cookie（`__Host-session`、Secure・HttpOnly・SameSite=Lax）に入れます。DBにはトークンのSHA-256だけを保存します。有効期限はログインから7日で、使っても延長しません。1人10件まで（超えると古いものから削除）。期限切れのセッションは1時間ごとに削除します。
- **表示するサーバー**：ログイン時に参加していて、運営者の許可リストに入っていて、Botが参加しているサーバーだけです。一覧はログイン時点のものなので、新しく参加したサーバーはログインし直すと表示されます。
- **権限**：`/talk` と同じ判定です。ロール設定を変更できるのはオーナーと「管理者」「サーバー管理」権限を持つ人だけで、Botの利用には利用ロールが必要です。メンバーのロールと権限はBotトークンでDiscordから取得し、60秒（サーバーのロールの定義とオーナーは5分。Discordでロールが作成・変更・削除されると即破棄）キャッシュします。ロールを外された人の画面上の権限は最大60秒残ります。ロールの設定自体はDBから毎回読むので、保存すると `/talk` にもすぐ反映されます。
- **ロール設定の保存**：利用ロールとナレッジ管理ロールを1回の操作でまとめて置き換えます（各25個まで。サーバーに存在しないロールは保存できません）。Discordで削除済みのロールが設定に残っていた場合は、保存すると外れます。
- **安全対策**：インラインスクリプトを使わず、外部のスクリプト・画像を読み込まない Content-Security-Policy、`X-Content-Type-Options: nosniff` などのヘッダーを付けます。GET以外のリクエストは `Origin` が `PUBLIC_BASE_URL` と一致しなければ拒否し、JSONの送信には `Content-Type: application/json` を必須にしています（CSRF対策）。HTTPS・HSTSはCaddyが担当します。
- **あなたのデータ**：画面上部の「あなたのデータ」（`/#/privacy`）で、自分について保存されているデータの件数を確認し、確認の手順を経て削除できます（`/privacy delete` と同じ処理。サーバーの権限がなくてもログインしていれば使え、削除するとログアウトされます）。API は `GET /api/privacy` と `POST /api/privacy/delete`（本文 `{"confirm":"DELETE"}`）です。
- **ページ**：`/privacy` と `/terms` で [docs/privacy.md](docs/privacy.md) と [docs/terms.md](docs/terms.md) を表示します（イメージに組み込むので、変更はイメージの再ビルドで反映されます）。`/healthz` は外部監視用です（DBとDiscordへの接続が正常なら200）。
- 画面はビルド不要の素のHTML/JS/CSS（`static/`）で、バイナリに組み込まれます。Webチャットの回答の表示に [marked](https://github.com/markedjs/marked) と [DOMPurify](https://github.com/cure53/DOMPurify) を `static/vendor/` に同梱しています（ライセンスは `static/vendor/LICENSES.txt`）。

### 手元で試す

Developer Portal の Redirects に `http://localhost:8080/auth/callback` も追加し、`.env` で `PUBLIC_BASE_URL=http://localhost:8080` と `WEB_BIND=127.0.0.1:8080` を設定して `cargo run` します（このときCookieは `__Host-` なし・Secureなしになります）。

## Webチャット

[Web管理画面](#web管理画面)の「チャット」で、Botの利用を許可されたサーバーごとにAIと会話できます（Open WebUI 風）。使えるのは `/talk` と同じく、そのサーバーの利用ロールを持つ人だけです（サーバー一覧の「チャット」ボタン、または画面上部の「チャット」から開きます）。Web管理画面を有効にすれば使え、追加の設定は必須ではありません。

- **画面**：左にサーバーの選択、「新しい会話」、会話の一覧（名前の変更・削除）、右に会話と入力欄があります。Enter で送信、Shift+Enter で改行（日本語の変換中の Enter では送信しません）。生成中は送信ボタンが「停止」になります。幅の狭い画面では「会話一覧」ボタンで一覧を開きます。
- **オプション**：メッセージごとに「Web検索」（`/talk` の `web_search` と同じ）と「ナレッジ」を選べます。「ナレッジ」は、そのサーバーに「利用できます」の資料があるときだけ表示され、既定でオンです。検索できなかったときや資料が見つからなかったときは、`/talk` と同じく回答の末尾に付記します。参照したWeb資料とナレッジ資料は回答の下に一覧で表示します（回答本文には付けません）。
- **文脈**：同じ会話の過去のやり取りを、新しいものから最大20件（10往復）・24,000文字までAIに渡します（回答は1件4,000文字まで）。失敗・中断した回答とその質問は含めず、停止した回答は途中までの本文を含めます。Discordのチャンネルの投稿や `/talk` の履歴は参照しません。会話の名前は、最初のメッセージの先頭40文字になります（AIは使いません）。
- **表示**：回答は生成されるそばから表示します（Server-Sent Events。15秒ごとに keep-alive）。回答のMarkdownは marked でHTMLにし（Markdownに書かれたHTMLは文字として表示）、DOMPurify で無害化してから表示します。画像・フォーム・スタイル・iframe・SVG・MathML は表示せず、リンクは http(s) だけを、新しいタブで（`rel="noopener noreferrer nofollow"`）開きます。質問は常に文字として表示します。回答はボタンでコピーできます。
- **停止・中断**：停止ボタン、タブを閉じる（接続が切れる）、Botの停止・再起動、`REQUEST_TIMEOUT_SECONDS` の経過のいずれかで生成を止め、それまでの本文を「停止」「中断」「失敗」の印とともに保存します。再起動時に生成中のまま残っていた回答は「中断」になります。自動で再生成はしません。
- **制限**：メッセージは1〜4,000文字です。生成は1人1件ずつで（別のタブで生成中なら「作成中です」と表示）、Bot全体では `/talk` と合わせて同時に4件までです（満杯なら「混み合っています」と表示し、メッセージは保存しません）。1人が直近24時間に送れるメッセージは `WEB_DAILY_MESSAGES_PER_USER`（既定100件）までで、会話を削除しても数は減りません（Botを再起動した後は、削除済みの会話の分は数えません）。1つの会話に送れるのは100件までです。
- **権限**：会話の一覧・作成・送信のたびに、そのサーバーでの利用権限を確認します。ロールを外されると最大60秒で、`ops guild deny` やBotがサーバーから外されたときは直ちに使えなくなります。会話は本人だけのもので、他人の会話は存在しないものとして扱います（404）。
- **保存**：会話（質問・回答・参照したWeb資料とナレッジ資料・オプション・状態）はDBに保存し、最終更新から `RETENTION_DAYS`（既定30日）で自動で削除します（1時間ごとの処理）。会話は一覧からいつでも削除でき、メッセージもすべて削除されます。
- **送信先**：`/talk` と同じく、質問・同じ会話の過去のやり取り・検索結果・ナレッジ資料の抜粋をOllama Cloudへ送ります（[docs/privacy.md](docs/privacy.md)）。ログには質問・回答の本文を記録しません。

## ナレッジベース

サーバーごとに資料（テキスト・Markdown・PDF）を登録しておくと、`/talk` がその内容を参考にして回答し、使った資料の名前を回答の末尾に表示します。資料の登録・削除は [Web管理画面](#web管理画面) の「ナレッジ」タブで、そのサーバーの「ナレッジ管理」ロールを持つ人とサーバー管理者だけが行えます（Discord からは登録できません）。利用ロールだけを持つ人には、資料の管理画面は表示されません。

**任意の機能**です。`EMBEDDING_PROVIDERS` が空なら無効で、`/talk`・Web管理画面はこれまでどおり動き、「ナレッジ」タブも表示されません。

> **注意**: 登録した資料の内容は、そのサーバーで `/talk` を使う人への回答（チャンネルに公開されます）に引用されることがあります。個人情報や外部に出せない情報を含む資料は登録しないでください。

### 仕組み

- **登録**：ファイルの種類は拡張子と先頭のバイトで判定します（ブラウザーが送る種類は信用しません）。テキストと Markdown は UTF-8 のみです。PDF は Bot とは別のプロセスで本文を取り出します（壊れた PDF で Bot が止まらないように、60秒で打ち切り、同時に1件だけ）。取り出した本文だけをDBに保存し、**元のファイルは保存しません**。文字化け（空、または読めない文字が10%を超える）のときは登録しません。受け付けている途中のファイルはメモリに置くため、Bot 全体で同時に2件までしか受け付けません（3件目は「ほかの資料の登録を処理中です」と表示され、しばらくしてから再試行できます）。
- **取り込み**：本文を約600文字ずつ（前後約100文字を重ねて）に分け、設定したすべての埋め込みプロバイダーで768次元のベクトルに変換して保存します。分ける位置は段落の終わり・文末（。！？など）を優先し、PDF の行の折り返しではなるべく分けません。処理はバックグラウンドで送信ペース（下記）を守りながら進み、Web 画面にプロバイダーごとの進み具合（処理済みのチャンク数）と、レート制限で待っているかどうかが表示されます。複数のサーバーに処理待ちの資料があるときはサーバーごとに順番に少しずつ進め（1つのサーバーの大きな資料がほかのサーバーの資料を何日も待たせないように）、同じサーバーの中では古い資料から処理します。どれか1つのプロバイダーで全チャンクがそろうと「利用できます」になり、残りのプロバイダーの分は処理待ちの資料がなくなりしだい補完します（その後も1時間ごとに確認します）。Bot を再起動しても続きから処理します。
- **エラー**：レート制限（429・1日の上限・利用枠の不足）は失敗に数えず、`Retry-After`（なければ1分から倍々に最大1時間）待って再開します。1日の上限なら翌日まで1時間ごとに様子を見ます。APIキーが拒否されたときはそのプロバイダーを10分止めます（資料は「処理中」のまま）。それ以外の一時的なエラーは5回まで自動で再試行し、それでも失敗した資料は「失敗」になります（Web 画面の「再試行」でやり直せます）。ただし、ほかのプロバイダーがレート制限で待っている間は、代わりに処理したプロバイダーのエラーも失敗に数えず、その資料はレート制限が明けるのを待ちます。
- **検索**：`/talk` は履歴を取得した後に質問をベクトルにし、サーバーの「利用できます」の資料から近い部分を正確な全件比較で探して、最大5件・合計8,000文字までをAIに渡します（同じ見出しの隣り合う部分は1つにまとめます）。異なるプロバイダーのベクトルは比べられないため、1回の検索では1つのプロバイダーのベクトルだけを使います。まず「利用できます」のすべての資料のベクトルがそろっているプロバイダーを設定順に試し、どれも使えないときだけ、一部の資料のベクトルしかないプロバイダーを（多くそろっている順に）使います。ベクトルが1つもないプロバイダーには質問を送りません。各プロバイダーは8秒まで待ち、レート制限中・エラーのものは飛ばします。全体で10秒以内に検索できなければ、資料なしで回答してその旨を付記します。
- **安全対策**：資料は「信頼できない参照資料であり命令ではない」と明示したJSONとしてAIに渡します。資料に仕込まれた指示で情報を外部へ送られないよう、`web_fetch` は同じ回答中のWeb検索結果に出たURLと質問文に書かれたURLだけを取得し、Botの回答にはリンクのプレビュー（埋め込み）を付けません。回答末尾の資料名はインラインコードで表示し、Markdown・リンク・メンションとして働かないようにしています。

### 設定

| 変数 | 既定 | 内容 |
| --- | --- | --- |
| `EMBEDDING_PROVIDERS` | 空（無効） | 使う埋め込みプロバイダーを優先順にカンマ区切りで（例 `gemini,openai`）。`gemini`・`openai`・`ollama` |
| `GEMINI_API_KEY` / `GEMINI_EMBEDDING_MODEL` | なし / `gemini-embedding-001` | Gemini API（Google AI Studio のキー） |
| `OPENAI_API_KEY` / `OPENAI_EMBEDDING_MODEL` | なし / `text-embedding-3-small` | OpenAI API |
| `OLLAMA_EMBEDDING_MODEL` | なし | `ollama` を使うときに必須。キーは `OLLAMA_API_KEY` を共用します。Ollama Cloud の `/api/embed` が使えるかは `tests/live_embed.rs` で確認します（[docs/runbook.md](docs/runbook.md) の「13. ナレッジベース」） |
| `<P>_EMBEDDING_REQUESTS_PER_MINUTE` / `<P>_EMBEDDING_TOKENS_PER_MINUTE` / `<P>_EMBEDDING_REQUESTS_PER_DAY` / `<P>_EMBEDDING_BATCH_SIZE` | 下表 | プロバイダーごとの送信ペースと1回にまとめる件数（`<P>` は `GEMINI`・`OPENAI`・`OLLAMA`） |
| `KB_MAX_UPLOAD_BYTES` | `5242880`（5 MiB） | 1ファイルの上限（64 KiB〜20 MiB）。Caddy の受信上限にも同じ値が使われます（`compose.yaml` が渡します） |
| `KB_MAX_DOCS_PER_GUILD` / `KB_MAX_CHUNKS_PER_GUILD` / `KB_MAX_CHUNKS_TOTAL` | `50` / `5000` / `30000` | サーバーごとの資料数・チャンク数と、Bot 全体のチャンク数の上限（約600文字で1チャンク） |

- `EMBEDDING_PROVIDERS` に挙げたプロバイダーのキー（とモデル）がないと、Bot は起動エラーになります。
- 異なるモデルのベクトルは比較できないため、保存するベクトルには「プロバイダー:モデル」（例 `gemini:gemini-embedding-001`）を付けます。モデルを変えると新しいモデルのベクトルが毎時の補完で作られ、古いものは `ops kb prune-embeddings` で消すまで残ります（[docs/runbook.md](docs/runbook.md)）。
- PDF に対応しないビルドは `cargo build --no-default-features`（cargo feature `pdf` を外す）で作れます。本番 VM では `scripts/build-image.sh --no-pdf` です。

### 送信ペース（無料枠で試すとき）

取り込みはプロバイダーごとに、直近1分・1日に送った量を数えて上限を超えないように送ります。1つのテキスト（チャンク）を1リクエストと数え（32件まとめて送っても32と数えます）、トークン数は文字数から多めに見積もります（日本語は1文字1トークン、英数字は3文字1トークン）。

| プロバイダー | 1分あたりのリクエスト | 1分あたりのトークン | 1日あたりのリクエスト（0は無制限） | 1回の件数 |
| --- | --- | --- | --- | --- |
| Gemini | 50 | 20,000 | 800 | 32 |
| OpenAI | 500 | 200,000 | 0 | 32 |
| Ollama | 60 | 30,000 | 0 | 32 |

- Gemini の既定値は、`gemini-embedding-001` の無料枠（1分100リクエスト・30,000トークン、1日1,000リクエスト）に収まるように選んでいます。`batchEmbedContents` の各要素が1リクエストと数えられる場合でも超えないよう、1テキストを1リクエストと数え、`/talk` の質問のベクトル化（1回の `/talk` で1リクエスト）の分も残しています。
- この既定値では、日本語の資料で1分に約30チャンク、1日に約800チャンク（本文で約40万文字）を処理します。大きな資料は数日かけて取り込まれます。Web 画面で進み具合と待ち時間を確認できます。
- `/talk` の質問のベクトル化がレート制限に当たったときは次のプロバイダーを使い、どれも使えなければ資料なしで回答して付記します。
- 有料枠では上限が大きいので、各変数を引き上げます（例: `GEMINI_EMBEDDING_REQUESTS_PER_MINUTE=1000`、`GEMINI_EMBEDDING_TOKENS_PER_MINUTE=500000`、`GEMINI_EMBEDDING_REQUESTS_PER_DAY=0`）。値はプロバイダーの管理画面に表示される上限より小さくしてください。

> **Gemini の無料枠についての注意**: Gemini API の無料枠では、送信した内容が Google のサービス改善に使われることがあります。無料枠で試すときは、公開しても問題のないテスト用の資料だけを使ってください。本番では課金を有効にしたプロジェクト（有料枠）を使います。

### 制限

- 登録できるファイル：`.txt`・`.text`・`.md`・`.markdown`（UTF-8）、`.pdf`（5 MiB・300ページまで。テキストを選択・コピーできるもの。画像だけの PDF やパスワード付きの PDF は登録できません。フォントを埋め込んでいない古い形式の日本語 PDF（CID フォント）も文字を取り出せないため、理由を表示して断ります。ブラウザーやワープロで開いて PDF として保存し直すと登録できます）。
- 1ファイル `KB_MAX_UPLOAD_BYTES`（既定 5 MiB）まで、取り出した本文は500,000文字までです。PDF は `KB_MAX_UPLOAD_BYTES` を大きくしても 5 MiB までです（PDF の解析は Bot と同じコンテナのメモリを使うため）。
- ファイル名に制御文字や、表示の向きを変える文字（U+202E など）は使えません。
- 同じ内容のファイル（SHA-256 が同じ）は、同じサーバーに2回登録できません。
- 資料の数とチャンク数の上限（上の表）は、登録の時点で確認します。
- 資料のプレビュー（取り出した本文の先頭2,000文字）で、文字化けしていないか確認できます。

## 履歴・検索・保存の仕様

- 参照期間は `/talk` の呼び出し日時を基準とし、開始時刻を含み、呼び出し時刻以降の投稿は含みません。スレッドと親チャンネルは独立しています。
- 通常投稿は必要時にDiscordから取得し、常時収集・DB保存はしません。対象は人間のテキスト投稿と本Botの回答です。他Botと外部Webhookは除外します。
- 過去の `/talk` の質問はMariaDBから取得します。Bot回答はDiscordに現存するメッセージを使うため、再起動しても参照できます。削除されたBot回答をDBから復活させることはありません。質問はDBの保持期間中は残ります。
- 時系列に整列し、ID重複を除き、最新100件・本文合計60,000 Unicode文字までAIに渡します。Discord取得は1回100件、最大10ページです。混雑したチャンネルなどで上限に達したら省略を明示します。発言者と日時も渡します。
- `history:0m` は通常投稿・過去の質問の両方を参照しません。現在の質問・回答は保存します。
- `web_search:false` ではツール定義をモデルに渡さず、モデルがツール呼び出しを返しても実行しません。
- `web_search:true` ではAIが必要に応じてOllamaの検索・ページ取得APIを呼びます。最大5ツール実行、検索1回5件、検索本文1件4000文字・取得ページ8000文字を上限とします。取得した出典URL一覧を回答末尾に付けます。検索が不要と判断された場合は実行しません。
- ページ取得（`web_fetch`）は、同じ回答中のWeb検索結果に出たURLと、質問文に書かれたURLだけを対象にします。それ以外のURLはリクエストを送らずに拒否し、「一部に失敗」と付記します（履歴・資料・取得したページに書かれた指示でデータを外部に送られないように）。
- Botの回答メッセージにはリンクのプレビュー（埋め込み）を付けません。
- AIに渡す会話、検索語、ページURLはOllama Cloudへ送信されます。ツールにはWeb検索・ページ取得だけを公開し、DB・シェル・Discord操作は公開しません。
- DBには質問、回答、出典URL・タイトル、ナレッジを参照したかどうかとAIに渡した資料のIDと名前、ID、オプション、処理状態、返信IDを保存します。通常投稿、取得ページ本文、内部推論、Interactionトークンは保存しません。
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
cargo test --locked --test database guild_role_writes_are_serialized -- --ignored --exact
cargo test --locked --test web web_sessions -- --ignored --exact
cargo test --locked --test web concurrent_logins_of_one_user_all_succeed -- --ignored --exact
cargo test --locked --test web healthz_reports_database_and_gateway -- --ignored --exact
cargo test --locked --test web web_login_keeps_only_allowlisted_guilds -- --ignored --exact
cargo test --locked --test web web_role_settings -- --ignored --exact
cargo test --locked --test knowledge knowledge_vectors -- --ignored --exact
cargo test --locked --test knowledge knowledge_quotas -- --ignored --exact
cargo test --locked --test knowledge knowledge_ingest -- --ignored --exact
cargo test --locked --test knowledge knowledge_worker_failover_and_backfill -- --ignored --exact
cargo test --locked --test knowledge knowledge_worker_restart_and_rate_limits -- --ignored --exact
cargo test --locked --test knowledge knowledge_worker_failures_and_deletion -- --ignored --exact
cargo test --locked --test web knowledge_web_api -- --ignored --exact
cargo test --locked --test chat web_chat -- --ignored --exact
cargo test --locked --test chat web_chat_knowledge_notices -- --ignored --exact
cargo test --locked --test chat web_chat_stops_with_partial_text -- --ignored --exact
cargo test --locked --test chat web_chat_retention_and_recovery -- --ignored --exact
bash scripts/check-vector-dump.sh
cargo test --locked --test web privacy_and_guild_purge -- --ignored --exact
Remove-Item Env:TEST_DATABASE_URL
```

ホストのRust環境を使わない場合は、上記の各 `cargo test` を次の形式で実行できます。

```powershell
docker run --rm --network discussion-bot-test_default -e TEST_DATABASE_URL=mysql://test:test_only_password@db-test:3306/discussion_test discord-discussion-bot:test --test database database_lifecycle -- --ignored --exact
```

再起動後はテスト名を `persistence_after_restart` に変更します。検証用データを破棄する場合だけ、`docker compose -f compose.test.yaml -p discussion-bot-test down -v` を実行してください。

`scripts/check-vector-dump.sh` は、`knowledge_vectors` が残すベクトルを `mariadb-dump --hex-blob`（`scripts/backup.sh` と同じ）で書き出して別のスキーマに復元し、VECTOR の値が1バイトも変わらないことを確かめます（Git Bash など bash で実行します）。

実際のOllama接続・検索を確認するテストも用意しています。`.env` のキーを使い、固定のテスト質問だけを送信してAPI利用枠を消費します。通常の `cargo test` では実行されません。

```powershell
cargo test --locked --test live_ollama -- --ignored
```

埋め込みAPI（Ollama・Gemini・OpenAI）を確認するテストは、`.env` にキーがあるプロバイダーだけについて、固定の公開テキストを送り、使えるかどうかを表示します。

```powershell
cargo test --locked --test live_embed -- --ignored --nocapture
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
8. Webチャット：回答が少しずつ表示されること、停止ボタンで途中までの回答が残ること、生成中にタブを閉じると生成も止まり（再度開くと「停止」で保存されている）、2つ目のタブからの送信は「作成中です」になること、`/talk` と合わせて4件を超えると「混み合っています」になること、XSS（`<img src=x onerror=alert(1)>` や `[x](javascript:alert(1))` を回答させても画像・スクリプトとして働かない）、開発者ツールのコンソールにCSP違反が出ないこと、利用ロールを外すと60秒以内に送信できなくなること。
9. `/privacy`：許可リストにないサーバーやロールのない人でも `show` が使えること、`delete` の確認ボタンで削除され、Web画面もログアウトされること（Web画面の「あなたのデータ」からの削除でも同様）、確認から10分を過ぎたボタンは期限切れになること。Botをサーバーから外すと `ops guild list` に `[bot left …]` と表示され、猶予内に招待し直すと元どおり使えること、猶予を過ぎるとそのサーバーのデータが削除され、許可リストから外れること。
10. ナレッジベースを有効にした場合：日本語の PDF を登録してプレビューが文字化けしていないこと、上限を超えるファイル・同じファイルの再登録で理由が表示されること、利用ロールだけの人には「ナレッジ」タブが表示されないこと、`/talk` の回答末尾に「参照したナレッジ資料:」と資料名が表示されること、`knowledge:false` で参照しないこと、取り込み中に Bot を再起動しても続きから処理されること、Gemini のキーを無効にすると OpenAI に切り替わること、PDF の処理中のメモリ（`docker stats`）。

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

本番で復元するときは、削除台帳（`ops privacy ledger export` / `apply`）でセルフ削除を適用し直します（[docs/runbook.md](docs/runbook.md)「バックアップと復元」）。バックアップには会話が含まれるため、アクセスを制限して保管してください。本番の `docker compose down -v` はDBボリュームを削除するので、通常の停止では使いません。DBパスワードを `.env` だけで変更しても、既存ボリューム内のDBユーザーのパスワードは変更されません。

## 参照仕様

- [Ollama Cloud直接接続とモデル名](https://docs.ollama.com/cloud)
- [Ollama Chat API](https://docs.ollama.com/api/chat)
- [Ollama Web検索・ページ取得](https://docs.ollama.com/capabilities/web-search)
- [Discord Gateway / Message Content Intent](https://docs.discord.com/developers/events/gateway)
