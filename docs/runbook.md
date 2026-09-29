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

**復元の練習**は手元の PC で行います（VM で 2 つ目の DB を動かすとメモリが足りません）。

```bash
age -d -i discussion-backup-identity.txt discussion-XXXX.sql.zst.age | zstd -d > restore.sql
docker compose -f compose.test.yaml -p discussion-bot-test up -d --wait
docker compose -f compose.test.yaml -p discussion-bot-test exec -T db-test sh -c 'MYSQL_PWD=test_only_root_password mariadb -uroot discussion_test' < restore.sql
```

**本番への復元**（Bot は止めておく）: 手元で復号したものを、ssh 経由で流し込みます。

```bash
age -d -i discussion-backup-identity.txt discussion-XXXX.sql.zst.age | zstd -d \
  | ssh <vm> 'cd ai-chat-for-discord && sudo docker compose exec -T db sh -c '\''MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot "$MARIADB_DATABASE"'\'''
```

**復元した後は**、「6. 削除依頼への対応」の台帳にある削除を、もう一度すべて適用します。

## 6. 削除依頼への対応

利用者から自分のデータの削除を依頼されたときの手順です（ナレッジ機能の段階で `/privacy` コマンドによるセルフサービスに置き換えます）。

1. 依頼者の Discord ユーザー ID を確認する。
2. 削除する（回答の投稿記録 `talk_replies` は外部キーで一緒に消えます）。

   ```bash
   sudo docker compose exec db sh -c 'MYSQL_PWD="$MARIADB_ROOT_PASSWORD" mariadb -uroot "$MARIADB_DATABASE" -e "DELETE FROM talk_runs WHERE user_id = <USER_ID>"'
   ```

3. バックアップからの復元に備えて、台帳に記録する（root だけが読めるファイル）。

   ```bash
   echo "$(date -u +%F) <USER_ID>" | sudo tee -a /etc/discussion-bot/erasures.log >/dev/null
   ```

4. 依頼者に完了を伝える。バックアップには最長 35 日残ることも伝える。

## 7. Discord Developer Portal

1. **Installation** → Install Link を **None** にする。
2. **Bot** → **Public Bot** を OFF にする（Install Link が残っていると OFF にできません）。
3. **Privileged Gateway Intents** → Message Content Intent は ON のまま。
4. **General Information** で次の 2 つを登録する。Web 管理画面の公開後は `https://<ドメイン>/privacy` などに切り替えます。
   - Privacy Policy URL: `https://github.com/mugicomugi/ai-chat-for-discord/blob/main/docs/privacy.md`
   - Terms of Service URL: 同じ場所の `terms.md`

## 8. サーバー（ギルド）を追加する

Public Bot が OFF のときは、アプリの所有者しか Bot を追加できません。

1. 追加先のサーバーの管理者に、運営者のアカウントへ「サーバー管理」権限のあるロールを一時的に付けてもらう。
2. 運営者が次の URL を開いて追加する（`<APP_ID>` と `<GUILD_ID>` を置き換える）。

   ```
   https://discord.com/oauth2/authorize?client_id=<APP_ID>&scope=bot%20applications.commands&permissions=274877975552&guild_id=<GUILD_ID>&disable_guild_select=true&integration_type=0
   ```

3. 許可リストへ追加する（M1 以降: `sudo docker compose exec bot /app/bot ops guild allow <GUILD_ID>`）。
4. 一時的に付けたロールを外してもらう。

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

- **ポート**: Web 管理画面を公開するとき（M3）は、OCI のセキュリティリストで TCP 80/443 だけを開けます。
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
- Web 管理画面の公開後（M3）は、外部の死活監視サービスから `https://<ドメイン>/healthz` を監視します。
