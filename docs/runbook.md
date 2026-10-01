# 運用手順書（本番 VM）

本番は Oracle Cloud の VM.Standard.E2.1.Micro（実効 1/8 OCPU・1GB RAM）で動かします。Bot のイメージはこの VM でビルドします。

- `scripts/cargo-vm.sh` と `scripts/build-image.sh` は、cargo をメモリ上限（既定 400MB を超えると抑制、550MB で上限）と最低の CPU・I/O 優先度で動かします。ビルドは遅い（初回は 1 時間以上）ので、利用の少ない時間に実行してください。
- イメージのタグは `discord-discussion-bot:git-<コミット>` です。GitHub Actions が使える場合は、CI が GHCR に push したイメージ（`…@sha256:…`）も同じ手順でデプロイできます。

- リポジトリの場所: `/home/ubuntu/ai-chat-for-discord`（以下のコマンドはこのディレクトリで実行）
- `docker` の実行には `sudo` が必要です。
- `compose.yaml` は `.env` の `BOT_IMAGE` が空だと、どのコマンド（`ps`・`logs`・`exec` を含む）も動きません。
- 本番稼働後は、VM 上で VS Code Server や Claude Code を常駐させないでください（約 360MB を使い、DB と Bot が swap に追い出されます）。

## 1. 初回セットアップ（ホスト）

1. `.env` を所有者だけが読めるようにする（Bot のトークンや DB のパスワードを含むため）。

   ```bash
   chmod 600 .env
   ```

2. 必要なパッケージを入れる。

   ```bash
   sudo apt-get update && sudo apt-get install -y age zstd
   ```

3. swap を使いにくくする（ページキャッシュを先に手放す）。

   ```bash
   sudo cp deploy/sysctl/99-discussion-bot.conf /etc/sysctl.d/
   sudo sysctl --system
   ```

4. バックアップ用の age 鍵を**手元の PC で**作る。

   ```bash
   age-keygen -o discussion-backup-identity.txt   # 表示される公開鍵 age1... を控える
   ```

   `discussion-backup-identity.txt`（秘密鍵）はパスワードマネージャーに保管し、VM には置きません。

5. 運用用の設定ファイルを作る（root だけが読めるようにする）。

   ```bash
   sudo install -d -m 700 /etc/discussion-bot
   sudo install -m 600 deploy/ops.env.example /etc/discussion-bot/ops.env
   sudoedit /etc/discussion-bot/ops.env   # BACKUP_AGE_RECIPIENT と OPS_WEBHOOK_URL を設定
   ```

   - このファイルは systemd と bash の両方が読みます。空白や `&`・`#`・`$` を含む値は、例のとおりダブルクォートで囲みます。
   - `OPS_WEBHOOK_URL` には、運営者専用の Discord サーバーのチャンネル Webhook を指定します。
   - `BACKUP_REMOTE_CMD` を設定すると、バックアップを VM の外（OCI Object Storage など）へもコピーします。保管先は 35 日で自動削除する設定にします。

6. ビルドに必要なものを入れ、リポジトリを更新してイメージをビルドし、`.env` に `BOT_IMAGE` を書く。

   ```bash
   sudo apt-get install -y --no-install-recommends gcc libc6-dev
   curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain 1.98 -c rustfmt,clippy
   git pull --ff-only
   scripts/build-image.sh            # 最後に表示されるタグを控える
   nano .env                         # BOT_IMAGE=discord-discussion-bot:git-<コミット> を追加
   ```

7. systemd のユニットを入れて有効にする。

   ```bash
   sudo cp deploy/systemd/*.service deploy/systemd/*.timer /etc/systemd/system/
   sudo systemctl daemon-reload
   sudo systemctl enable --now discussion-bot-imds-block.service \
       discussion-bot-backup.timer discussion-bot-healthwatch.timer
   systemctl list-timers 'discussion-bot-*'
   ```

   `discussion-bot-imds-block` は、コンテナから OCI のメタデータサービス（169.254.0.0/16）へ通信できないようにします（DNS だけは通します）。

8. 使っていない常駐サービスを止める（メモリの節約）。

   ```bash
   sudo systemctl disable --now fwupd-refresh.timer
   sudo systemctl mask --now fwupd.service packagekit.service
   ```

   - snapd は止めないでください（OCI の Ubuntu では Oracle Cloud Agent が snap で動いています）。
   - iscsid と multipathd もブートボリュームに必要です。
   - Oracle Cloud Agent のプラグインのうち不要なもの（Run Command など）は、OCI コンソールのインスタンス詳細画面「Oracle Cloud Agent」タブで無効にします。

9. （GitHub Actions を使う場合だけ）GHCR のパッケージを公開にする。リポジトリは公開済みで、イメージに秘密情報は含まれません。
   - 手順: GitHub の Packages → `ai-chat-for-discord` → Package settings → Change visibility → Public。
   - 非公開のままにする場合は、`read:packages` 権限だけの classic PAT（期限 90 日）を作り、`sudo docker login ghcr.io -u mugicomugi --password-stdin` でログインします。

## 2. デプロイと切り戻し

> **M0 の初回（DB がまだ 11.4 のとき）は、この手順ではなく「3. MariaDB 11.4 → 12.3.3 への更新」を行います。**

1. VM のリポジトリをデプロイするコミットに合わせる（compose.yaml やスクリプトがイメージと一致するように）。

   ```bash
   git pull --ff-only          # または git checkout <commit>
   ```

2. テストしてからイメージをビルドする（どちらも時間がかかるので、利用の少ない時間に）。

   ```bash
   scripts/cargo-vm.sh clippy --locked --all-targets -- -D warnings
   scripts/cargo-vm.sh test --locked
   scripts/build-image.sh      # 最後に discord-discussion-bot:git-<コミット> が表示される
   ```

3. デプロイする。

   ```bash
   sudo scripts/deploy.sh discord-discussion-bot:git-<コミット>
   ```

   `deploy.sh` は次の順に処理します。
   1. 暗号化したバックアップを取る（`backups/pre-deploy`）。
   2. イメージがあることを確認する（CI のイメージなら pull する）。
   3. `.env` の `BOT_IMAGE` を書き換える。
   4. **Bot だけ**を再起動する（DB は作り直しません）。
   5. `database_ready` と `discord_ready` がログに出るまで待つ。
   6. 古いイメージを整理する（直近 3 つは残す）。

4. **切り戻し**: `sudo tail backups/deploy-history.log` の `previous=` の値を指定して、同じコマンドを実行します。マイグレーションは追加だけなので、古いイメージも新しいスキーマで起動できます。直近 3 つのイメージを残しています。

   > **M1 より前のイメージに戻すと、サーバーの許可リストとロールによる利用制限が効かなくなります。** Bot を追加しているサーバーの全員が `/talk` を使える状態に戻ります。DB の設定は残るので、M1 以降のイメージに戻せば再び有効になります。

   > **M2 より前のイメージに戻すと、Web 管理画面は止まります**（Caddy は動き続け、502 を返します）。ログイン中のセッションは DB に残り、M2 以降のイメージに戻せばそのまま使えます。Docker のヘルスチェックも M2 のイメージから入ったものなので、古いイメージでは `docker compose ps` に health が表示されません。

   > **M3 より前のイメージに戻すと、ナレッジベースは使えなくなります**（`/talk` は資料なしで答え、Web 画面に「ナレッジ」タブが出ません）。資料・チャンク・ベクトルは DB に残り、M3 以降のイメージに戻せば取り込みの続きから再開します。

**M0 以前のイメージ（`docker compose up --build` でビルドしていた頃のもの）に戻す場合**は、`deploy.sh` が受け付けないタグなので、次のように戻します。

```bash
sudo docker image ls          # 以前のイメージ名（例: ai-chat-for-discord-bot:latest）を確認
nano .env                      # BOT_IMAGE=<そのイメージ名> に書き換える
sudo docker compose up -d --no-build --no-deps bot
```

## 3. MariaDB 11.4 → 12.3.3 への更新（一度だけ）

事前に「1. 初回セットアップ」を済ませておきます（`.env` の `BOT_IMAGE` を含む）。

1. Bot を止め、11.4 が動いているうちに論理バックアップを取る。

   ```bash
   sudo docker compose stop bot
   sudo scripts/backup.sh pre-deploy
   ```

2. DB を止め、ボリュームを丸ごと暗号化して保存する（コールドバックアップ）。

   ```bash
   sudo docker compose stop db
   sudo docker volume ls | grep mariadb_data     # 名前を確認（例: ai-chat-for-discord_mariadb_data）
   sudo bash -c 'set -euo pipefail; set -a; . /etc/discussion-bot/ops.env; set +a; umask 077
     vol=ai-chat-for-discord_mariadb_data; docker volume inspect "$vol" >/dev/null
     mkdir -p backups/pre-upgrade; out=backups/pre-upgrade/mariadb_data-11.4.tar.zst.age
     docker run --rm --network none -v "$vol":/data:ro debian:trixie-slim tar -C /data -cf - . \
       | zstd -q -T1 | age -r "$BACKUP_AGE_RECIPIENT" -o "$out"
     ls -lh "$out"'
   ```

   `vol=` の値は、直前に確認したボリューム名に合わせます。最後に表示されるサイズが MB 単位であること（数百バイトではないこと）を確認します。

3. 12.3.3 で DB を起動する（`MARIADB_AUTO_UPGRADE=1` により `mariadb-upgrade` が一度だけ走ります）。

   ```bash
   sudo docker compose up -d db
   sudo docker compose logs -f db      # healthy になるまで待つ（Ctrl+C で抜ける）
   ```

4. データベースの既定の照合順序を、表と同じ `utf8mb4_unicode_ci` にそろえる（今後の表の既定値だけが変わり、既存の表やデータは変わりません）。

   ```bash
   sudo docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot -e "ALTER DATABASE \`$MARIADB_DATABASE\` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"'
   ```

5. 確認する。

   ```bash
   sudo docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot -e "SELECT @@version, @@transaction_isolation, @@collation_server"'
   sudo docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot -e "SELECT DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = \"$MARIADB_DATABASE\""'
   sudo docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot -e "SELECT VEC_DISTANCE_COSINE(0x0000803f, 0x0000803f) AS d"'
   ```

   `12.3.3-MariaDB`、`READ-COMMITTED`、`utf8mb4_unicode_ci`（2 か所）、`d = 0` になれば成功です。

6. Bot を起動して、`/talk`（Web 検索あり・なし）を試す。

   ```bash
   sudo scripts/deploy.sh "$(sed -n 's/^BOT_IMAGE=//p' .env)"
   ```

7. 問題がなければ、**35 日以内に** `backups/pre-upgrade/` を削除する（プライバシーポリシーの保持期間に合わせるため）。

   ```bash
   sudo rm -r backups/pre-upgrade
   ```

**12.3 から戻す場合**（ダウングレードはサポートされないため、ボリュームごと戻します）:

1. `sudo docker compose stop`（`down -v` は絶対に使わない）
2. 手元の PC で `discussion-backup-identity.txt` を使って tar を復号し、VM へ送る。
3. 空にしたボリュームへ展開する。
4. compose.yaml の db イメージを `mariadb:11.4` に戻す。
5. 起動する。

作業は取り違えのないよう 2 人で確認するか、先に手元の PC で予行してください。

## 4. マイグレーションが途中で失敗したとき

ログに `database_migration_failed kind="dirty" version=N` と出て、Bot が再起動を繰り返す場合の手順です。

1. `sudo docker compose stop bot`
2. 状態を確認する。

   ```bash
   sudo docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot "$MARIADB_DATABASE" -e "SELECT version, description, success FROM _sqlx_migrations WHERE success = 0"'
   ```

3. `migrations/` の該当ファイルは DDL を 1 文しか含みません。対象の表や列がすでにできているかを確認します。
   - できていなければ、失敗した行を削除する（`DELETE FROM _sqlx_migrations WHERE version = N AND success = 0`）。
   - できていれば、その DDL を手作業で取り消してから行を削除する。
4. 原因を直したイメージをデプロイするか、直前のイメージに切り戻す。判断がつかないときは `backups/pre-deploy` の最新のものから復元します（「5. バックアップと復元」）。

## 5. バックアップと復元

**自動で取るもの**
- 毎日 03:30（JST）: `backups/daily`（7 世代）。日曜（JST）のものは `backups/weekly`（4 世代）にも残す。
- デプロイ前: `backups/pre-deploy`（3 世代）。
- どれも 35 日より古いものは削除する。
- `BACKUP_REMOTE_CMD` を設定していれば、VM の外にもコピーする。

形式はすべて `mariadb-dump | zstd | age` です。VM の外の保管先にも 35 日で消える設定をし、プライバシーポリシーの「バックアップは最長 35 日」と一致させます。

ナレッジベースのベクトル（`kb_embeddings.embedding`、VECTOR 型）は、`mariadb-dump --hex-blob` でなければ正しく書き出せません。`scripts/backup.sh` はこのオプションを使っており、CI（`scripts/check-vector-dump.sh`）が、書き出して別のスキーマに復元したベクトルが元と1バイトも違わないことを毎回確認しています。手作業でダンプするときも `--hex-blob` を付け、ベクトルの表に `SELECT … INTO OUTFILE` / `LOAD DATA` は使わないでください（MDEV-40853）。

**復元の練習**は手元の PC で行います（VM で 2 つ目の DB を動かすとメモリが足りません）。

```bash
age -d -i discussion-backup-identity.txt discussion-XXXX.sql.zst.age | zstd -d > restore.sql
docker compose -f compose.test.yaml -p discussion-bot-test up -d --wait
docker compose -f compose.test.yaml -p discussion-bot-test exec -T db-test sh -c 'MYSQL_PWD=test_only_root_password mariadb -uroot discussion_test' < restore.sql
```

**本番への復元**は次の順に行います。

1. **削除台帳を書き出す**（今の DB が読める場合）。利用者が `/privacy delete` などで削除した記録（ユーザー ID と日時）は DB の `privacy_erasures` に 40 日（バックアップの最長 35 日より長い）残っています。復元するとバックアップ時点の台帳に戻ってしまうので、先に書き出しておきます。中身はユーザー ID なので、root だけが読める場所に置きます。

   ```bash
   sudo sh -c 'umask 077 && docker compose exec -T bot /app/bot ops privacy ledger export > /root/erasures-$(date -u +%F).tsv'
   ```

   DB が読めない（ディスクの故障など）場合は書き出せないので飛ばします。このとき、最後のバックアップより後の削除は適用し直せません（復元したバックアップに入っている台帳の分は、すでにバックアップに反映されています）。

2. **Bot を止めて復元する**: 手元で復号したものを、ssh 経由で流し込みます。

   ```bash
   ssh <vm> 'cd ai-chat-for-discord && sudo docker compose stop bot'
   age -d -i discussion-backup-identity.txt discussion-XXXX.sql.zst.age | zstd -d \
     | ssh <vm> 'cd ai-chat-for-discord && sudo docker compose exec -T db sh -c '\''MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot "$MARIADB_DATABASE"'\'''
   ```

3. **Bot を起動する前に**、次の 2 つを行います。

   1. 削除台帳を適用し直す。Bot は止めたままなので、`exec` ではなく `run` で一時的なコンテナを使います（このコマンドだけはマイグレーションも行うので、台帳の表がない古いバックアップでも動きます）。台帳の各利用者について、削除した日時までに作られたデータを削除・匿名化し直します（その後に作られたデータは残します）。何度実行しても結果は同じです。

      ```bash
      sudo sh -c 'docker compose run --rm --no-deps -T bot ops privacy ledger apply - < /root/erasures-<日付>.tsv'
      # 「6. 削除依頼への対応」の資料の削除の記録（と M5 より前の旧台帳）があれば、それも
      sudo sh -c 'docker compose run --rm --no-deps -T bot ops privacy ledger apply - < /etc/discussion-bot/erasures.log'
      ```

   2. Web 管理画面のログインをすべて消す（バックアップの時点で有効だったセッションが復活し、その後にログアウトしたものや、漏えいのため全員をログアウトさせたものも再び使えてしまうため）。利用者はもう一度ログインすれば使えます。

      ```bash
      sudo docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot "$MARIADB_DATABASE" -e "DELETE FROM web_sessions"'
      ```

4. `sudo docker compose up -d bot` で起動する。書き出した台帳のファイルは、復元が済んだら削除します（`sudo rm /root/erasures-<日付>.tsv`）。

## 6. 削除依頼への対応

利用者は、自分のデータを自分で削除できます（README の「自分のデータの確認と削除」）。

- Discord: `/privacy show` で件数を確認し、`/privacy delete` の確認ボタンで削除する。許可リストやロールに関係なく使えます。
- Web: 画面上部の「あなたのデータ」（`https://<ドメイン>/#/privacy`）。ログインできれば、サーバーの権限がなくても使えます。

削除するのは `/talk` の記録（返信の記録を含む）・Web チャットの会話（メッセージを含む）・Web のログインで、ナレッジ資料とロール設定は資料・設定を残して登録者（設定者）の ID と名前だけを消します。生成中の Web チャットの回答は止まり、保存されません。回答を作成中の `/talk` の記録だけは残るので、その場合は少し待ってからもう一度削除してもらいます。削除はログ `privacy_erased`（件数だけ）に出て、DB の台帳（`privacy_erasures`、40 日で自動削除）に記録されます。台帳は「5. バックアップと復元」で使います。

**運営者が代わりに削除するとき**（Discord もログインも使えない人からの依頼など）

1. 依頼者の Discord ユーザー ID を確認する。
2. 削除する（`/privacy delete` と同じ処理で、台帳にも記録されます）。

   ```bash
   sudo docker compose exec bot /app/bot ops privacy erase <USER_ID>
   ```

3. ナレッジ資料の内容に依頼者の個人情報が含まれている場合は、そのサーバーのナレッジ管理者に資料の削除を依頼するか、運営者が削除します（チャンクとベクトルも一緒に消えます）。運営者が削除した資料は台帳に入らないので、復元に備えて資料 ID を root だけが読めるファイルに控えます（「5. バックアップと復元」で一緒に適用されます）。

   ```bash
   sudo docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot "$MARIADB_DATABASE" -e "DELETE FROM kb_documents WHERE id = <ID>"'
   echo "$(date -u +%F) kb_document=<ID>" | sudo tee -a /etc/discussion-bot/erasures.log >/dev/null
   ```

4. 依頼者に完了を伝える。バックアップには最長 35 日残り、復元した場合も削除し直すことも伝える。

### M5 を初めてデプロイしたとき（一度だけ）

M5 より前は、削除した利用者を `/etc/discussion-bot/erasures.log` に手で記録していました（`<日付> <USER_ID>` と `kb_document=<ID>`）。デプロイ後に一度だけ適用し、利用者の分を DB の台帳へ移します。各利用者について、記録した日の終わりまでに作られたデータだけを削除し直すので、その後にまた使い始めた人のデータは消えません。

```bash
sudo sh -c 'docker compose exec -T bot /app/bot ops privacy ledger apply - < /etc/discussion-bot/erasures.log'
```

以後、利用者の削除をこのファイルに書く必要はありません。ファイルは、運営者が削除した資料の記録（上の 3.）のために残します。40 日より古い利用者の行は、台帳に移っても次の 1 時間ごとの処理で消えます（35 日より古いバックアップはないため、もう必要ありません）。

## 7. Discord Developer Portal

1. **Installation** → Install Link を **None** にする。
2. **Bot** → **Public Bot** を OFF にする（Install Link が残っていると OFF にできません）。
3. **Privileged Gateway Intents** → Message Content Intent は ON のまま。
4. **General Information** で次の 2 つを登録する。Web 管理画面を公開した後は `https://<ドメイン>/privacy` と `https://<ドメイン>/terms` に切り替えます（「12. Web 管理画面を公開する」）。
   - Privacy Policy URL: `https://github.com/mugicomugi/ai-chat-for-discord/blob/main/docs/privacy.md`
   - Terms of Service URL: 同じ場所の `terms.md`

## 8. サーバー（ギルド）を追加する

Public Bot が OFF のときは、アプリの所有者しか Bot を追加できません。

1. 追加先のサーバーの管理者に、運営者のアカウントへ「サーバー管理」権限のあるロールを一時的に付けてもらう。
2. 運営者が次の URL を開いて追加する（`<APP_ID>` と `<GUILD_ID>` を置き換える）。

   ```
   https://discord.com/oauth2/authorize?client_id=<APP_ID>&scope=bot%20applications.commands&permissions=274877975552&guild_id=<GUILD_ID>&disable_guild_select=true&integration_type=0
   ```

3. 許可リストへ追加する。

   ```bash
   sudo docker compose exec bot /app/bot ops guild allow <GUILD_ID> <メモ（任意）>
   ```

4. 利用ロールを決める。**利用ロールが未設定のサーバーでは誰も `/talk` を使えません。**
   - 通常は、そのサーバーの「サーバー管理」権限を持つ人に `/config role-add role:<ロール>` を実行してもらう。
   - 運営者が代わりに設定する場合は `ops guild role add <GUILD_ID> use <ROLE_ID>`（下記）。全員に許可するときは ROLE_ID に GUILD_ID（@everyone）を指定する。
5. 一時的に付けたロールを外してもらう。

### 許可リストとロールの操作（ops コマンド）

稼働中の Bot コンテナの中で実行します（Bot と同じ DB とトークンを使い、Discord への常時接続はしません）。ロール ID は Discord の開発者モードでロールを右クリックしてコピーします。

```bash
ops() { sudo docker compose exec bot /app/bot ops "$@"; }
ops guild list                               # 許可リストと、Bot が参加しているサーバーの一覧
ops guild allow <GUILD_ID> [メモ]             # 許可する（拒否していた場合は解除）
ops guild deny <GUILD_ID>                    # 許可を取り消す（ロールの設定は残る）
ops guild role list <GUILD_ID>               # 設定済みのロール
ops guild role add <GUILD_ID> use <ROLE_ID>       # 利用ロールを追加
ops guild role add <GUILD_ID> manage <ROLE_ID>    # ナレッジ管理ロールを追加
ops guild role remove <GUILD_ID> use <ROLE_ID>    # 外す
ops guild purge <GUILD_ID> [--force]         # Bot が外されたサーバーのデータを今すぐ削除（下記）
ops kb prune-embeddings [--apply]            # 今の設定にないモデルのベクトルを削除（「13-5」）
ops privacy erase <USER_ID>                  # 利用者のデータを削除（「6. 削除依頼への対応」）
ops privacy ledger export                    # 削除台帳を書き出す（「5. バックアップと復元」）
```

`ops privacy ledger apply -` は標準入力から読むので、`ops` 関数ではなく `sudo docker compose exec -T bot /app/bot ops …` の形で使います（「5. バックアップと復元」）。

- 種類ごとに最大 25 ロールです。Discord で削除したロールは自動で設定から外れます。
- `ops guild list` の名前の後ろの `[bot left <日付>]` は、Bot がそのサーバーから外された日です。

### Bot がサーバーから外されたとき

Bot がサーバーから外される（キックされる、サーバーが削除される）と、その日時を `guilds.left_at` に記録します（ログ `guild_left`。Bot が止まっている間に外された場合は、次に Discord へ接続したときに `guild_left_while_offline`）。

- **猶予**: `GUILD_PURGE_GRACE_DAYS`（既定 14 日、1〜365）の間は何も消しません。猶予中に再び招待されれば記録が消え（ログ `guild_rejoined`）、許可リスト・ロール設定・ナレッジ資料はそのまま使えます。
- **削除**: 猶予を過ぎると、1 時間ごとの処理（起動直後の 1 回を除く）がそのサーバーの `/talk` の記録・Web チャットの会話・ナレッジ資料（チャンクとベクトルを含む）・ロール設定を削除し、許可リストからも外します（ログ `left_guild_purged`。`guilds` の行は `purged <日付> after the bot left` というメモ付きで残ります）。その後に招待し直したときは、「8. サーバー（ギルド）を追加する」の 3. からやり直します。バックアップには最長 35 日残ります。
- **すぐに消す**: 猶予を待たずに消すときは `ops guild purge <GUILD_ID>`。Bot がまだ参加しているサーバーは、`--force` を付けない限り拒否します（`--force` で消した場合も許可リストから外れます）。
- ロールの設定を変えられるのは、`/config` ではサーバーのオーナーと「管理者」「サーバー管理」権限を持つ人だけです（ナレッジ管理ロールでは変更できません）。

### M1 を初めてデプロイしたとき（一度だけ）

M1 から、許可リストに入っていて利用ロールが設定されたサーバーでしか `/talk` が使えません。**デプロイした直後は、すべてのサーバーで誰も使えなくなる**ので、続けて次を行います。

1. `ops guild list` で、Bot が参加しているサーバーを確認する（STATUS がすべて `not-allowed` になっている）。
2. 使い続けるサーバーごとに `ops guild allow <GUILD_ID>` を実行する。
3. 利用ロールを設定する。各サーバーの管理者に `/config role-add` を依頼するか、運営者が `ops guild role add` で設定する。これまでどおり全員に使わせる場合は `ops guild role add <GUILD_ID> use <GUILD_ID>`（@everyone）。
4. `ops guild role list <GUILD_ID>` で確認し、テスト用のアカウントで `/talk` が応答することを確かめる。

## 9. 古いギルドコマンドの掃除（一度だけ）

以前のバージョンは、`.env` に `DISCORD_GUILD_ID` があると、そのサーバーにだけ `/talk` をギルドコマンドとして登録していました。今のバージョンはグローバルコマンドとして登録するので、そのサーバーでは `/talk` が 2 つ表示されます。`.env` に `DISCORD_GUILD_ID` を設定していた場合は、一度だけ次を実行して、そのサーバーのギルドコマンドを消します（`<APP_ID>` は Developer Portal の Application ID）。

```bash
sudo bash -c 'set -euo pipefail; set -a; . ./.env; set +a
  printf "Authorization: Bot %s\n" "$DISCORD_TOKEN" \
  | curl -fsS -X PUT -H @- -H "Content-Type: application/json" -d "[]" \
    "https://discord.com/api/v10/applications/<APP_ID>/guilds/${DISCORD_GUILD_ID}/commands" >/dev/null && echo cleaned'
```

トークンがコマンドラインやシェルの履歴に残らないよう、ヘッダーは標準入力から渡しています。実行後は `.env` から `DISCORD_GUILD_ID` を消してかまいません。

## 10. OCI での注意

- **ポート**: Web 管理画面を公開するとき（「12. Web 管理画面を公開する」）は、OCI のセキュリティリストで TCP 80/443 だけを開けます。
  - ホストの iptables の INPUT は変更しません。Docker が公開するポートは FORWARD を通るためです。
  - Docker が動いている間に `netfilter-persistent save` を実行しないでください。
  - 8080（Bot）と 3306（DB）は公開しません。
- **アイドル回収**: Always Free のインスタンスは、7 日間の CPU・ネットワーク・メモリの使用率が低いと回収されることがあります。
  - テナンシーを従量課金（PAYG）に切り替える（Always Free の範囲は引き続き無料）ことを検討してください。
  - VM を作り直せるよう、この手順書と VM の外のバックアップを保ってください。
- **保存時の暗号化**: OCI のブートボリュームとブロックボリュームは、既定で AES-256 により暗号化されます。バックアップは age で暗号化します。

## 11. 監視

- `discussion-bot-healthwatch.timer`（5 分ごと）が次を確認し、異常を `OPS_WEBHOOK_URL` に通知します（同じ内容は 6 時間に 1 回まで。送信に失敗したら次回に再送）。
  - コンテナの状態・health・再起動回数
  - ディスク使用率（85% 超）
  - 空きメモリ（100MB 未満）
  - バックアップの鮮度（26 時間超）
  - TLS 証明書の期限（`DOMAIN` を設定した後）
- 状態の確認: `systemctl list-timers 'discussion-bot-*'`、`journalctl -u discussion-bot-healthwatch -n 50`
- Bot コンテナには Docker のヘルスチェック（`/app/bot healthcheck`、30 秒ごと）があり、Web 管理画面を有効にしていると DB と Discord への接続を確かめます（無効なら常に healthy）。unhealthy になると healthwatch が通知します。ヘルスチェックは `compose.yaml` ではなくイメージ（`Dockerfile`）に入っているので、M2 より前のイメージに切り戻すとヘルスチェックなしで動きます（古いバイナリは `healthcheck` を通常の起動として扱うため、`compose.yaml` に書いてはいけません）。
- Web 管理画面の公開後は、外部の死活監視サービスから `https://<ドメイン>/healthz` を監視します（「12. Web 管理画面を公開する」）。

## 12. Web 管理画面を公開する

Discord でログインしてロールを設定できる画面（README の「Web管理画面」）を `https://<ドメイン>/` で公開します。Caddy が HTTPS の証明書（Let's Encrypt）を自動で取得・更新し、Bot の 8080 番へ中継します。Bot の 8080 番と DB はホストに公開しません。

**メモリ**: Caddy の上限は 96MB です（Bot 256MB・DB 512MB と合わせて 1GB の VM に収まる見積もり）。公開後しばらくは `sudo docker stats --no-stream` と healthwatch の空きメモリの通知を確認してください。

### 公開の手順（一度だけ）

1. **ドメインと DNS**: ドメイン（例 `bot.example.com`）の A レコードを VM のパブリック IP に向ける。IPv6 で公開しない場合は AAAA レコードを作らない。`dig +short bot.example.com` で反映を確認する。
2. **OCI のポート**: VCN のセキュリティリスト（または NSG）で、インバウンドの **TCP 80 と 443**（ソース `0.0.0.0/0`）を許可する。
   - 80 は証明書の取得（HTTP-01）と HTTPS へのリダイレクトに使います。UDP 443（HTTP/3）は使いません。
   - ホストの iptables は変更しません（「10. OCI での注意」）。
3. **Developer Portal**（アプリの **OAuth2** 画面）
   - **Redirects** に `https://<ドメイン>/auth/callback` を追加して保存する。
   - **Client ID** を控え、**Client Secret** を発行（Reset Secret）して控える。
4. **`.env`** に追加する（`chmod 600 .env` のままにする）。

   ```dotenv
   DISCORD_CLIENT_ID=<Client ID>
   DISCORD_CLIENT_SECRET=<Client Secret>
   PUBLIC_BASE_URL=https://<ドメイン>
   DOMAIN=<ドメイン>
   COMPOSE_PROFILES=web
   ```

   3 つの変数（`DISCORD_CLIENT_ID`・`DISCORD_CLIENT_SECRET`・`PUBLIC_BASE_URL`）の一部だけを設定すると、Bot は起動しません。

5. **証明書の期限監視**: `sudoedit /etc/discussion-bot/ops.env` で `DOMAIN="<ドメイン>"` を設定する（healthwatch が証明書の期限を確認します）。
6. **起動**: Bot を作り直し（新しい環境変数を読ませる）、Caddy を起動する。

   ```bash
   sudo scripts/deploy.sh "$(sed -n 's/^BOT_IMAGE=//p' .env)"
   sudo docker compose up -d caddy
   sudo docker compose logs --tail 50 caddy     # "certificate obtained successfully" を確認
   ```

   ログに `web_listening` が出ていれば、Bot は 8080 番で待ち受けています。

7. **確認**
   - `curl -sS https://<ドメイン>/healthz` が `ok` を返す。`curl -sI http://<ドメイン>/` が HTTPS へのリダイレクト（308）になる。
   - ブラウザーで `https://<ドメイン>/` を開き、Discord でログインする。開発者ツールで、Cookie `__Host-session` が Secure・HttpOnly・SameSite=Lax であることと、コンソールに CSP の違反が出ていないことを確認する。
   - サーバー管理権限のあるアカウントでロールを保存し、`/config show` に反映されること、権限のないアカウントでは設定画面が出ないことを確認する。Discord でロールを外すと、60 秒以内に Web 画面の権限からも外れる。
   - `sudo docker compose ps` で bot が `healthy`、caddy が `running` であること。healthwatch は caddy も監視します。
8. **外部からの監視**: UptimeRobot などの外部の死活監視サービスで `https://<ドメイン>/healthz` を 5 分ごとに監視し、200 以外で運営者に通知する設定にする（VM ごと止まった場合は healthwatch では通知できないため）。本文は `ok` / `unavailable` だけで、詳細は出しません。
9. **Privacy Policy / Terms of Service の URL**: `docs/privacy.md` と `docs/terms.md` の `<…>` を埋めてコミットし、そのイメージをデプロイしてから、Developer Portal の **General Information** を `https://<ドメイン>/privacy` と `https://<ドメイン>/terms` に切り替える。2 つのページはイメージに組み込まれるので、文面の変更はイメージの再ビルドで反映されます。

### 運用

- **Caddyfile（`deploy/caddy/Caddyfile`）を変更したとき**: `sudo docker compose restart caddy`（管理 API を無効にしているので `caddy reload` は使えません）。
- **Caddy の更新**: Dependabot が `compose.yaml` の digest を更新する PR を作ります。マージ後に `git pull --ff-only` と `sudo docker compose up -d caddy`。
- **証明書**: `caddy_data` ボリュームに保存されます。消すと再取得になり、Let's Encrypt の発行回数の上限に当たることがあるので、`docker compose down -v` は使いません。
- **Client Secret の交換**: Developer Portal で Reset Secret し、`.env` を書き換えて `sudo scripts/deploy.sh "$(sed -n 's/^BOT_IMAGE=//p' .env)"`。ログイン中のセッションはそのまま使えます。
- **全員をログアウトさせる**: `sudo docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot "$MARIADB_DATABASE" -e "DELETE FROM web_sessions"'`

### Web チャット

Web 管理画面を公開すると、利用ロールを持つ人は Web チャット（README の「Webチャット」）も使えます。追加の設定は必須ではありません。

- **Ollama の利用量**: 1 人が直近 24 時間に送れるメッセージは `WEB_DAILY_MESSAGES_PER_USER`（既定 100 件）までです。変えるときは `.env` に書いて Bot を作り直します（`sudo scripts/deploy.sh "$(sed -n 's/^BOT_IMAGE=//p' .env)"`）。生成は `/talk` と合わせて同時に 4 件まで、1 回の回答は `REQUEST_TIMEOUT_SECONDS` までです。
- **ストリーミング**: 回答は Server-Sent Events で送ります。Caddyfile の `flush_interval -1` が必要です（外すと、回答が最後にまとめて表示されます）。
- **ログ**: `web_chat_started`・`web_chat_finished`（`status` が `completed`／`stopped`、`reason` が `stopped`／`disconnected`）・`web_chat_failed`（`error_code`）・`web_chat_daily_limit` を記録します。質問・回答の本文は記録しません。
- **保存期間**: 会話は最終更新から `RETENTION_DAYS` で、1 時間ごとの処理（ログ `conversation_cleanup`）が削除します。
- **停止・再起動**: 生成中の回答は、Bot の停止時に途中までの本文を「中断」として保存します。保存できなかったものも、次の起動時に「中断」になります（ログ `database_ready` の `interrupted`）。自動で再生成はしません。
- **公開をやめる**: `.env` の 3 つの変数を空にして `COMPOSE_PROFILES` を消し、Bot を作り直す（上の deploy.sh）。続けて `sudo docker compose --profile web stop caddy` と `sudo docker compose --profile web rm -f caddy` で Caddy を止め、OCI のセキュリティリストから 80/443 を外す。最後に `sudoedit /etc/discussion-bot/ops.env` で `DOMAIN=""` にする（残っていると、healthwatch が証明書を確認できないという通知を 6 時間ごとに送り続けます）。

## 13. ナレッジベース

サーバーごとの資料を `/talk` が参照する機能です（README の「ナレッジベース」）。資料は Web 管理画面からだけ登録するので、先に「12. Web 管理画面を公開する」を済ませておきます。`EMBEDDING_PROVIDERS` が空の間は無効です。

### 13-1. Ollama の埋め込みが使えるかの確認（一度だけ）

既存の `OLLAMA_API_KEY` で `https://ollama.com/api/embed` が使えるかを、固定の公開テキストだけを送って確かめます（利用枠をわずかに使います）。

```bash
scripts/cargo-vm.sh test --locked --test live_embed live_ollama_embed -- --ignored --nocapture
```

初回はテスト用のビルドに時間がかかります（利用の少ない時間に実行してください）。`Ollama: WORKS with <モデル>: 768 dimensions …` と出れば使えます。モデルは `.env` の `OLLAMA_EMBEDDING_MODEL`（なければ `embeddinggemma`）です。`DOES NOT WORK` の場合は、表示された種類（`embedding_auth`: キーが拒否された、`embedding_upstream`: モデルがない・サーバーエラー、`embedding_invalid`: 768 次元で返らない など）を控えます。

**結果の記録**（実行したら書き換える）:

| 実行日 | モデル | 結果 |
| --- | --- | --- |
| 2026-10-02（JST、検証用 LXC） | `embeddinggemma` | 利用不可。`live_ollama_embed` は文書・質問とも `embedding_auth` で失敗。追加の直接確認でも `/api/embed` が HTTP 401 を返した。同じキーの `live_chat_and_search` は成功。Cloud の `/api/tags` に当該モデル・埋め込みモデルはなく、今回の環境では Gemini（主）と OpenAI（予備）を使う。 |

使えない場合は Gemini（主）と OpenAI（予備）を使います（`EMBEDDING_PROVIDERS=gemini,openai`）。

### 13-2. キーの準備

- **Gemini**: Google AI Studio（<https://aistudio.google.com/apikey>）で API キーを作ります。**本番では課金を有効にしたプロジェクト（有料枠）のキーを使います**。無料枠では送信した内容が Google のサービス改善に使われることがあるため、無料枠で試すときは公開しても問題のないテスト用の資料だけを使ってください。
- **OpenAI**: <https://platform.openai.com/api-keys> で API キーを作り、組織の設定（Limits）で**月額の利用上限（budget）**を設定します。権限を絞ったキー（Restricted）にする場合は、`/v1/embeddings` を呼べる権限だけを付けます。
- 確認（キーのあるプロバイダーだけ実行されます）:

  ```bash
  scripts/cargo-vm.sh test --locked --test live_embed -- --ignored --nocapture
  ```

### 13-3. 有効にする

1. `.env` に追加する（`chmod 600 .env` のまま）。

   ```dotenv
   EMBEDDING_PROVIDERS=gemini,openai
   GEMINI_API_KEY=<キー>
   OPENAI_API_KEY=<キー>
   ```

   - 並べた順が優先順です。取り込みも検索も先頭から使い、使えないものは飛ばします。
   - 並べたプロバイダーのキーがないと Bot は起動しません（`deploy.sh` が `discord_ready` を待ってタイムアウトします）。
2. Bot を作り直す: `sudo scripts/deploy.sh "$(sed -n 's/^BOT_IMAGE=//p' .env)"`。ログに `knowledge_worker_started` が出れば動いています。
3. `KB_MAX_UPLOAD_BYTES` を変えたときは Caddy も作り直す（受信上限が同じ値になります）: `sudo docker compose up -d caddy`。
4. 各サーバーの管理者に、Web 管理画面の「ロール設定」で「ナレッジ管理ロール」を必要に応じて設定してもらいます（サーバー管理権限を持つ人は設定なしで管理できます）。
5. 確認: Web 管理画面の「ナレッジ」タブで小さな資料を登録し、「利用できます」になったら `/talk message:<資料の内容についての質問> knowledge:true` で、回答の末尾に「参照したナレッジ資料:」が出ることを確かめます。PDF を登録している間に `sudo docker stats --no-stream` で Bot のメモリを確認します（PDF は Bot と同じコンテナの別プロセスで処理されます）。256MB に近づくようなら `KB_MAX_UPLOAD_BYTES` を下げるか、`compose.yaml` の Bot の `mem_limit` を 320m に上げます。

### 13-4. 送信ペースとレート制限

取り込み（バックグラウンド）は、プロバイダーごとに `<P>_EMBEDDING_REQUESTS_PER_MINUTE`・`<P>_EMBEDDING_TOKENS_PER_MINUTE`・`<P>_EMBEDDING_REQUESTS_PER_DAY`（`<P>` は `GEMINI`・`OPENAI`・`OLLAMA`）を超えないように送ります。既定値と考え方は README の「送信ペース」にあります。

- **Gemini の既定値は無料枠向け**です（1分50件・20,000トークン、1日800件。1日に約800チャンクしか進みません）。有料枠に切り替えたら、Google AI Studio のプロジェクトの上限を見て引き上げます（例 `GEMINI_EMBEDDING_REQUESTS_PER_MINUTE=1000`、`GEMINI_EMBEDDING_TOKENS_PER_MINUTE=500000`、`GEMINI_EMBEDDING_REQUESTS_PER_DAY=0`）。変更は Bot の作り直しで反映されます。
- 1日の件数の上限は取り込みだけに効き、`/talk` の質問のベクトル化（1回1件）はその残りを使います。Bot を再起動すると数え直しになりますが、そのときはプロバイダーの 429 で止まります。
- レート制限（429・1日の上限・利用枠の不足）は資料の失敗に数えません。Web 画面のプロバイダー欄に「レート制限のため HH:MM ごろまで待機」と表示され、時間が来ると自動で再開します（最長でも1時間ごとに様子を見ます）。
- ログ（本文・キーは出しません）:
  - `embedding_rate_limited`（`provider`, `wait_seconds`）: 429 などで一時停止した。
  - `embedding_auth_failed`: キーが拒否された（そのプロバイダーを10分止める）。キーと課金設定を確認します。
  - `embedding_request_failed`（`error_code`）: 一時的なエラー。同じ資料で5回続くと `kb_document_failed` になります（ほかのプロバイダーがレート制限・1日の上限で待っている間のエラーは数えず、資料はそのプロバイダーを待ちます）。
  - `knowledge_query_provider_skipped` / `knowledge_query_timeout` / `knowledge_search_failed`: `/talk` の検索で使えないプロバイダーがあった・検索できなかった。
  - `kb_upload_busy`: 同時に受け付けられる2件の登録が処理中で、資料の登録を断った。続くようなら登録の多い時間を避けてもらいます。
  - `pdf_extractor_failed`（`exit_code`, `signal`）: PDF を読む子プロセスが異常終了した。`signal` が出ているときはメモリ不足で止められた可能性が高いので、`sudo docker stats --no-stream` で Bot のメモリを確認します。

### 13-5. モデルを変えたとき（古いベクトルの削除）

ベクトルには「プロバイダー:モデル」が付いていて、異なるモデルのものは比較しません。`GEMINI_EMBEDDING_MODEL` などを変えると、新しいモデルのベクトルが補完で作られます（送信ペースに従うので時間がかかります）。その間、検索はすべての資料のベクトルがそろっているプロバイダー（`EMBEDDING_PROVIDERS` のほかのプロバイダー）を使い、そろったプロバイダーがないときだけ、新しいモデルのベクトルがある資料を対象にします。古いモデルのベクトルは検索に使わず、削除するまでそのまま残ります。

補完が終わったら（Web 画面で、すべての資料の新しいモデルの進み具合が全チャンクになったら）、古いベクトルを消します。

`ops` は「8. サーバー（ギルド）を追加する」で定義したシェル関数（`sudo docker compose exec bot /app/bot ops …`）です。

```bash
ops kb prune-embeddings             # 確認だけ（何件消えるかを表示し、何もしない）
ops kb prune-embeddings --apply     # 今の EMBEDDING_PROVIDERS にないキーのベクトルを削除
```

`EMBEDDING_PROVIDERS` が空のときは、すべてが「古い」と判定されてしまうため実行を拒否します。設定ミスで消えないよう、自動では削除しません。

### 13-6. Bot がサーバーから外されたとき

資料は、そのサーバーの他のデータと一緒に `GUILD_PURGE_GRACE_DAYS`（既定 14 日）の猶予の後で自動的に削除されます（チャンクとベクトルも一緒に消えます）。猶予を待たずに消すときは `ops guild purge <GUILD_ID>` です（「8. サーバー（ギルド）を追加する」の「Bot がサーバーから外されたとき」）。

### 13-7. 容量の上限

`KB_MAX_DOCS_PER_GUILD`（50件）・`KB_MAX_CHUNKS_PER_GUILD`（5,000チャンク）・`KB_MAX_CHUNKS_TOTAL`（Bot 全体で30,000チャンク）は登録の時点で確認します。検索は1回にそのサーバーの全ベクトルと比べる（ベクトル索引なし）ので、5,000チャンクで1回約15MBを読みます。DB のメモリ（512MB）に余裕がない場合は上限を下げます。上限を下げても登録済みの資料は消えません。
