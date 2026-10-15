# Run your own Sophia, and reach it from anywhere

This guide is for people who already use Sophia and want to run their own copy, for themselves and their family. You
download your gardens, run them with **gardend** (the Sophia engine) on a computer at home, and reach that computer from
your phone and your other computers through **Cloudflare**, which also handles the login.

The commands are for Terminal on a Mac or a shell on Linux. Paste them one block at a time. Where a command holds a value
that is yours (an email address, a domain, a date), change it before you press Enter. This guide uses
`sophia.example.com` as the address: use a hostname on your own domain.

## 1. What you get, and what you need

You get:

- your Sophia at `https://sophia.example.com`, on any phone or computer;
- a login by email code, run by Cloudflare Access: only the people you list can get in;
- the same address for your agents (Claude Code and others), through sophia-mcp;
- no open ports on your router. Your gardens stay on your computer.

Your connection is encrypted between your devices and Cloudflare, and between Cloudflare and your computer. Cloudflare
handles it in between, as it does for any site it serves.

You need:

- **a computer that stays on and awake**: a Mac, or a Linux machine (Ubuntu 22.04 or newer, or similar). On Windows, use
  WSL2 (section 13);
- **for chat, Node.js 22.19 or newer and a model key** from [OpenRouter](https://openrouter.ai) (section 11), and the same
  for workflows (Sophia Compute, section 12, off until you turn it on). Everything else works without them;
- **disk space**: downloads can be several GB, and you need room for the download and the unpacked copy at the same time;
- **a Cloudflare account** (the free plan is enough);
- **a domain on Cloudflare**: on the free plan, the domain's nameservers must point to Cloudflare.

## 2. Download your gardens

In Sophia, open **Settings → Account → Download my gardens**. Your browser saves a file named like
`sophia-gardens-2026-10-13.tar`.

- It holds every garden you own, exactly as Sophia stores it: documents, folders, comments, files, history and restore
  points, memory and the RDF store.
- Gardens that other people shared with you are not in it. Their owners can download them.
- The `README.md` inside lists the gardens it holds, and any it had to leave out and why.
- You can download up to three times a day.

## 3. Install gardend

gardend 0.1.2 or newer is needed: it is the first version that serves the Sophia app itself. Chat needs 0.1.4 or newer. The newest version is always on
the [releases page](https://github.com/sophia-labs/gardend/releases). Pick the build for your computer:

| Your computer | Build (`T`) |
|---|---|
| Mac with Apple silicon (M1 and later) | `aarch64-apple-darwin` |
| Mac with an Intel processor | `x86_64-apple-darwin` |
| Linux on a regular PC | `x86_64-unknown-linux-gnu` |
| Linux on ARM (Raspberry Pi 4 or 5 with a 64-bit system, ARM servers) | `aarch64-unknown-linux-gnu` |

Download it and check that it arrived intact. In the first two lines, put the newest version from the releases page and
your build from the table:

```bash
V=v0.1.4
T=aarch64-apple-darwin
cd ~/Downloads
curl -fLO https://github.com/sophia-labs/gardend/releases/download/$V/gardend-$V-$T.tar.gz
curl -fLO https://github.com/sophia-labs/gardend/releases/download/$V/SHA256SUMS
shasum -a 256 -c SHA256SUMS --ignore-missing
```

On Linux, the last line is `sha256sum -c SHA256SUMS --ignore-missing`. It should print the file name and `OK`. Now unpack it into your home folder as `~/gardend`, and make the `gardend` command
available everywhere:

```bash
tar -xzf gardend-$V-$T.tar.gz
mv gardend-$V-$T ~/gardend
sudo mkdir -p /usr/local/bin
sudo ln -sf ~/gardend/gardend /usr/local/bin/gardend
gardend --version
```

`~/gardend` holds the `gardend` program and, beside it, a `ui` folder (the Sophia app) and a `services` folder (chat,
section 11). Keep them together.

**On a Mac:** gardend is not signed with an Apple Developer ID. If you downloaded it with a browser instead of `curl`,
macOS may refuse to run it. Clear the download flag:

```bash
xattr -dr com.apple.quarantine ~/gardend
```

Or, after macOS shows its warning, open **System Settings → Privacy & Security** and choose **Open Anyway**.

## 4. Open your gardens

Make a private folder for your Sophia, unpack the download into it, and start Sophia. Change the date to match your file:

```bash
mkdir -p ~/sophia && chmod 700 ~/sophia
tar -xf ~/Downloads/sophia-gardens-2026-10-13.tar -C ~/sophia
ls ~/sophia
gardend serve --profile ~/sophia
```

`ls` should show `README.md` and `graphs`.

If your browser already unpacked the download for you, you have a folder named like `sophia-gardens-2026-10-13` that holds
those two. In that case, replace the first two lines with
`mv ~/Downloads/sophia-gardens-2026-10-13 ~/sophia && chmod 700 ~/sophia`.

When Sophia is ready, gardend prints:

```
Sophia is ready.

  Open Sophia: http://127.0.0.1:7674/#key=sophia-key-…
```

Open that link in a browser on this computer. Your gardens are there.

- `~/sophia` is now your Sophia. gardend adds its own files beside `graphs`, among them the key, `selfhost-key.json`. Keep
  the folder private: anyone who can read it can read your gardens.
- The key in the link opens everything. The browser remembers it, so you need the link once per browser. Do not share it.
- To see the link again: `gardend url --profile ~/sophia`.
- If port 7674 is taken, add `--port 7700` (or another number) here, to `gardend service install` in steps 5 and 7, and use
  that number everywhere below.
- Ctrl-C stops Sophia. It finishes what it is doing before it exits.

Once your gardens open, you can delete the `.tar` from Downloads.

## 5. Keep it running

Stop the `gardend serve` from step 4 with Ctrl-C. Then install Sophia as a background service:

```bash
gardend service install --profile ~/sophia
```

The service starts Sophia now and every time you log in, and restarts it if it crashes. It prints the open link again.

- **Mac:** it is a LaunchAgent, `~/Library/LaunchAgents/dev.sophia.gardend.plist`. The log is
  `~/Library/Logs/Sophia/gardend.log`. It runs while you are logged in, so after a restart, log in once (or turn on
  automatic login).
- **Linux:** it is a systemd user service, `~/.config/systemd/user/gardend.service`. Read its log with
  `journalctl --user -u gardend -f`. To keep it running when you are logged out, and to start it after a reboot:

  ```bash
  sudo loginctl enable-linger $USER
  ```

- `gardend service status` says whether it is running. `gardend service uninstall` removes the service and leaves
  `~/sophia` alone.
- Keep the computer from sleeping. On a Mac, in **System Settings**, open **Energy** (or **Battery → Options** on a laptop)
  and turn on the option that prevents automatic sleeping when the display is off.

## 6. Reach it from anywhere: a Cloudflare Tunnel

A tunnel is a small program, **cloudflared**, that runs on your computer and connects out to Cloudflare. Cloudflare then
passes visits to `sophia.example.com` through it to Sophia on `127.0.0.1:7674`. Nothing on your router changes.

In this step you make the tunnel, but you do not give it an address yet. The address comes in step 7, after the lock.

1. Install cloudflared. On a Mac with [Homebrew](https://brew.sh): `brew install cloudflared`. On Linux, the dashboard
   shows the commands in the next step.
2. In the Cloudflare dashboard, go to **Networking → Tunnels → Create Tunnel**. Name it `sophia`, and select
   **Create Tunnel**.
3. Under **Setup Environment**, pick this computer's system. Copy the commands under **Install and Run** and run them here.
   The last one looks like `sudo cloudflared service install <a long token>`: it runs cloudflared as a service that
   starts with the computer. The token is a secret: do not paste it anywhere else.
4. The tunnel shows **Healthy** on the Tunnels page. If the dashboard offers to add a route now, leave it for step 7.

## 7. Lock the door with Cloudflare Access, then publish the address

Cloudflare Access is the login. It asks each visitor for their email, sends them a one-time code, and lets in only the
addresses you list. It then attaches a signed pass to every request, and gardend checks that pass. Do all of this step
before anyone uses the address.

**7.1 Turn on email codes.** Go to **Zero Trust → Integrations → Identity providers → Add new identity provider →
One-time PIN**.

**7.2 Create the Access application.** Go to **Zero Trust → Access controls → Applications → Create new application →
Self-hosted and private → Add public hostname**. Enter the subdomain `sophia` and pick your domain. Then:

- under **Access policies**, create a policy named `People`, with the action **Allow** and an **Include** rule of
  **Emails** = your own email address;
- choose **One-time PIN** as the login method;
- set a **Session Duration** (the default is 24 hours; up to a month is handy on phones);
- select **Create**.

**7.3 Copy two values** into a note:

- the **AUD tag**: **Access controls → Applications**, select **Configure** on your application, then **Additional
  settings → Application Audience (AUD) Tag**;
- your **team domain**: **Zero Trust → Settings**. It looks like `yourteam.cloudflareaccess.com`.

**7.4 Publish the address.** Go to **Networking → Tunnels**, select `sophia`, then **Routes → Add route → Published
application**. Enter the hostname `sophia` on your domain, and the service URL `http://127.0.0.1:7674`. Select
**Add route**.

**7.5 Tell gardend about Access.** Keep the settings in a small file, so that every later `service install` has them.
Change the four values to yours before you paste:

```bash
mkdir -p ~/.config/sophia
cat > ~/.config/sophia/env <<'EOF'
export GARDEN_PUBLIC_BASE_URL=https://sophia.example.com
export GARDEN_CF_ACCESS_TEAM_DOMAIN=yourteam.cloudflareaccess.com
export GARDEN_CF_ACCESS_AUD=paste-the-aud-tag-here
export GARDEN_CF_ACCESS_ADMIN_EMAILS=you@example.com
EOF
```

Then reinstall the service with them. `service install` copies these settings from your Terminal into the service:

```bash
. ~/.config/sophia/env
gardend service install --profile ~/sophia
```

Its `env` line should list the four names. What they mean:

- `GARDEN_PUBLIC_BASE_URL` is your address, exactly as people type it. You need it: without it, gardend still starts but
  logs a warning, and refuses every change made in a browser through your address ("invalid origin").
- `GARDEN_CF_ACCESS_TEAM_DOMAIN` and `GARDEN_CF_ACCESS_AUD` tell gardend whose passes to trust. Set both: gardend refuses to
  start with only one.
- `GARDEN_CF_ACCESS_ADMIN_EMAILS` lists the admins, separated by commas. Everyone else Access lets in is a member
  (step 8). If you leave this line out, everyone you let in is an admin.

**7.6 Check it.** On this computer:

```bash
curl -s http://127.0.0.1:7674/self-host.json
```

The answer should include `"auth":"cloudflare-access"`. Then, on your phone, open `https://sophia.example.com`. Cloudflare
should ask for your email before anything else. **If Sophia opens without asking, stop, and fix the Access application
(7.2) before you go on.** Otherwise enter your email and the code Cloudflare sends you, and Sophia opens.

From now on, open Sophia at `https://sophia.example.com` on your phone and your other computers. On this computer, the
`127.0.0.1` link from step 4 keeps working: there the app uses its key, not Cloudflare's login. (That needs gardend 0.1.3 or
newer. With 0.1.2, the `127.0.0.1` link stops working once Access is on, so use your address on this computer too.)

## 8. Family

To let someone in, add their email to the `People` policy: **Access controls → Applications**, **Configure** on your
application, then edit the policy. They open `https://sophia.example.com`, type their email, and enter the code that
Cloudflare sends them from `noreply@notify.cloudflare.com`.

What people can do depends on `GARDEN_CF_ACCESS_ADMIN_EMAILS` (7.5):

- **Admins** (the emails in that list) can do everything.
- **Members** (everyone else you let in) can open and read every garden, search, write and edit documents, make and move
  folders, add files and images, link things together, and use Sophia's tools from an agent. They cannot delete documents,
  folders or gardens; create, rename, import or restore gardens; load or change raw RDF; make or apply restore points;
  write to Sophia's memory; or manage tokens, models and services.

Everyone you let in sees every garden on this Sophia: there is no sharing garden by garden here.

- To make someone an admin, add their email to `GARDEN_CF_ACCESS_ADMIN_EMAILS` in `~/.config/sophia/env`, then run
  `. ~/.config/sophia/env && gardend service install --profile ~/sophia` again.
- To remove someone, take their email out of the policy. To sign them out at once, go to **Zero Trust → Team & Resources →
  Users**, tick their name, and choose **Action → Revoke**.
- The free Cloudflare plan covers up to 50 people.

## 9. Agents and MCP

Agents reach Sophia through [sophia-mcp](https://github.com/sophia-labs/sophia-mcp) 0.6.2 or newer. Install it on the
computer the agent runs on (`T` is the same build name as in step 3):

```bash
V=v0.6.2
T=aarch64-apple-darwin
cd ~/Downloads
curl -fLO https://github.com/sophia-labs/sophia-mcp/releases/download/$V/sophia-mcp-$V-$T.tar.gz
tar -xzf sophia-mcp-$V-$T.tar.gz
sudo install -m 0755 sophia-mcp-$V-$T/sophia-mcp /usr/local/bin/sophia-mcp
sophia-mcp --version
```

**From anywhere, through Cloudflare.** Give each agent computer a Cloudflare **service token**:

1. Go to **Zero Trust → Access controls → Service credentials → Service Tokens → Create Service Token**. Name it after the
   computer, pick a duration, and select **Generate token**. Copy the **Client ID** and **Client Secret** into your password
   manager: the secret is shown only once.
2. Open your application (**Access controls → Applications → Configure**) and add a second policy, named `Agents`, with the
   action **Service Auth** and an **Include** rule of **Service Token** = the token you made. It must be **Service Auth**:
   with **Allow**, Cloudflare answers the agent with a login page.
3. In your MCP client's settings (for example a Claude Code `.mcp.json`), add:

   ```json
   {
     "mcpServers": {
       "sophia": {
         "command": "sophia-mcp",
         "args": ["--backend", "https://sophia.example.com/mcp"],
         "env": {
           "CF_ACCESS_CLIENT_ID": "<Client ID>",
           "CF_ACCESS_CLIENT_SECRET": "<Client Secret>"
         }
       }
     }
   }
   ```

The Client ID ends in `.access`. Cloudflare checks the token, and gardend reads who is calling from Cloudflare's signed
pass. While `GARDEN_CF_ACCESS_ADMIN_EMAILS` is set, an agent has a member's rights (step 8); without it, an admin's. No
Sophia key is needed. If you do set `SOPHIA_MCP_TOKEN` as well, gardend goes by that key instead, and the agent gets that
key's rights (see below).

- `--cf-access-client-id` and `--cf-access-client-secret` work too, but the settings above keep the secret off the
  computer's process list.
- To revoke an agent, delete its service token in Cloudflare.

**On the computer that runs Sophia**, an agent can talk to gardend directly, without Cloudflare. Make it a token of its own:

```bash
gardend token create --label my-agent --grant member --expires-days 365 --profile-dir ~/sophia
```

It prints the token once. Use `"args": ["--backend", "http://127.0.0.1:7674/mcp"]` and
`"env": { "SOPHIA_MCP_TOKEN": "<the token>" }`. A `member` token has a member's rights (step 8), Sophia's tools included.
An agent that must also delete things or load RDF needs `--grant session-all`, which has every right. `read-only` and
`authoring` tokens cannot use Sophia's tools. `--grant member` needs gardend 0.1.3 or newer: with 0.1.2, use
`--grant session-all`. `gardend token list --profile-dir ~/sophia` shows your tokens, and
`gardend token revoke <id> --profile-dir ~/sophia` turns one off at once. In `token` commands, `--grant` (or `--profile`)
is the level of access, and `--profile-dir` is the folder.

## 10. Updating and backups

**Back up** by stopping Sophia, copying the folder, and starting it again. Change the destination to your backup disk.

On a Mac, stop Sophia and check that it has stopped:

```bash
launchctl bootout gui/$(id -u)/dev.sophia.gardend
gardend service status
```

When the status says `not loaded`, copy the folder and start Sophia again:

```bash
cp -a ~/sophia /Volumes/Backup/sophia-$(date +%F)
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.sophia.gardend.plist
```

On Linux:

```bash
systemctl --user stop gardend
cp -a ~/sophia /mnt/backup/sophia-$(date +%F)
systemctl --user start gardend
```

gardend has no backup command of its own, and a copy taken while Sophia runs can catch a file half-written. Restore points are kept inside the same folder, so they are not
a backup. To restore, stop Sophia, put the copy back at `~/sophia`, and start it.

**Update gardend** with `gardend update`. Make a backup first. `gardend --check-update` says whether a newer version
exists. Sophia must be stopped while it updates. On a Mac:

```bash
launchctl bootout gui/$(id -u)/dev.sophia.gardend
gardend update
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.sophia.gardend.plist
gardend --version
```

On Linux:

```bash
systemctl --user stop gardend
gardend update
systemctl --user start gardend
gardend --version
```

`gardend update` downloads the newest release, checks it against the release's checksums, and replaces the `gardend`
program and the `ui` and `services` folders beside it. If it says Sophia is still running, wait a moment and run it again. It refuses a
release that stores gardens in a new format: then the release notes say what to do.

**If `gardend --version` says 0.1.3 or older**, its `gardend update` does not bring the `services` folder (and 0.1.2's
leaves the `ui` folder as it was too). This once, update by replacing the whole `~/gardend` folder instead. Download and check the new version with step 3's
first block (with the new number in `V`), and unpack it with `tar -xzf gardend-$V-$T.tar.gz`. Then, on a Mac:

```bash
launchctl bootout gui/$(id -u)/dev.sophia.gardend
rm -rf ~/gardend && mv gardend-$V-$T ~/gardend
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.sophia.gardend.plist
gardend --version
```

On Linux:

```bash
systemctl --user stop gardend
rm -rf ~/gardend && mv gardend-$V-$T ~/gardend
systemctl --user start gardend
gardend --version
```

**Loading a large RDF file** (a statistical cube, a whole catalogue: anything past the 500 MiB the app's import takes)
is `gardend load`. Stop Sophia, then:

```bash
gardend load --profile ~/sophia --graph <graph-id> data.nq.gz
```

The graph must exist already. The load streams the file (gzip too), commits once at the end, and refuses to run while
Sophia serves the folder. `gardend load --help` says more.

Keep cloudflared up to date too: `brew upgrade cloudflared` on a Mac, your package manager on Linux.

## 11. Chat

The chat panel runs on your computer too, from the `services` folder beside gardend, on a model you choose and pay for.
Nothing goes through Sophia's own servers. It needs gardend 0.1.4 or newer, Node.js, and a model key.

**Install Node.js 22.19 or newer.**

- Mac: `brew install node@22` with [Homebrew](https://brew.sh), then follow what it prints to put it on your PATH; or the
  installer from [nodejs.org](https://nodejs.org).
- Linux (and WSL2): from [NodeSource](https://github.com/nodesource/distributions) (`node_22.x`), or with
  [nvm](https://github.com/nvm-sh/nvm): `nvm install 22`.

In a new Terminal, check it:

```bash
node --version
```

It should print `v22.19.0` or newer. gardend looks for `node` on its PATH each time it starts. The service from step 5
keeps the PATH of the Terminal that installed it, so install it again from this Terminal:

```bash
. ~/.config/sophia/env
gardend service install --profile ~/sophia
```

(Leave out the first line if you skipped step 7.)

**Set your model key.** Chat uses [OpenRouter](https://openrouter.ai), which reaches most models with one key. Make a key
at [openrouter.ai/keys](https://openrouter.ai/keys), give it a monthly limit if you like, and then:

```bash
gardend chat key set --profile ~/sophia
```

It asks for the key, keeps it in `~/sophia/services.env` (readable only by you) and restarts chat. To remove it:
`gardend chat key clear --profile ~/sophia`.

Open Sophia: the chat panel is on the right.

- Chat answers from the garden that is open. It searches and reads its documents, and can write them, with the rights
  of a member (step 8). It does not run programs on your computer.
- Everyone you let in can chat, admins and members. Each person sees only their own conversations. They are kept in
  `~/sophia/services/vehicle`.
- Each reply stops after 12 model calls or $0.25, whichever comes first. The model is Gemini 2.5 Flash.
- If chat says **"Chat needs Node.js 22.19 or newer"**, gardend did not find it: install Node.js and install the service
  again, as above. Sophia works without chat in the meantime.
- Chat needs nothing more. Agent sessions, which can also run commands in a folder of their own, run inside a sandbox:
  on a Mac the one built into macOS, on Linux and WSL2 bubblewrap (`sudo apt install bubblewrap`).

## 12. Sophia Compute (workflows)

Sophia Compute runs workflows on your computer: programs of steps, some of them agents that read and write your gardens
with a model, from the `services/choreograph` folder beside gardend. It is **off until you turn it on**, because it
takes memory while it waits. It needs what chat needs (section 11): Node.js 22.19 or newer, and a model key.

```bash
gardend service enable choreograph --profile ~/sophia
```

gardend starts it now if it is running, and every time it starts from then on. `gardend service disable choreograph
--profile ~/sophia` stops it and keeps it off. A workflow your agents start through gardend (the `workflow_run_start`
tool, step 9) starts it when it is needed, even while it is off.

- It uses the model key chat uses (`gardend chat key set`). To give it a key of its own:
  `gardend service env set OPENROUTER_API_KEY --service choreograph --profile ~/sophia`.
- Waiting, it takes 100 to 160 MB of memory (the most just after it starts) and 1 to 2% of one processor core; while a
  workflow runs, each agent step runs as a short-lived process of its own.
- Its agent steps run on your computer as you, without a sandbox, and reach your gardens only through gardend, with the
  rights of a member. Only admins (you, with your link or as an Access admin) start workflows; members chat.
- Its records (runs, sessions, costs) are kept in `~/sophia/services/choreograph`.

## 13. On Windows: WSL2

gardend runs in WSL2, the Linux inside Windows, as it does on Linux.

1. In PowerShell: `wsl --install -d Ubuntu`. Restart if asked, and open **Ubuntu**.
2. Step 5's service needs systemd. Recent Ubuntu images have it on: `cat /etc/wsl.conf` shows `systemd=true` under
   `[boot]`. If not, in Ubuntu:

   ```bash
   printf '[boot]\nsystemd=true\n' | sudo tee -a /etc/wsl.conf
   ```

   Then, in PowerShell, `wsl --shutdown`, and open Ubuntu again. (`gardend service install` says the same when systemd
   is off.)
3. In Ubuntu, follow this guide with the Linux commands and the Linux build (`x86_64-unknown-linux-gnu`). Your Windows
   Downloads folder is `/mnt/c/Users/<your name>/Downloads`: copy the gardens download into Ubuntu
   (`mkdir -p ~/Downloads && cp /mnt/c/Users/<your name>/Downloads/sophia-gardens-*.tar ~/Downloads/`) and keep `~/sophia` in Ubuntu, not under
   `/mnt/c`. For agent sessions, also `sudo apt install bubblewrap` (section 11).
4. Open the link from step 4 in a browser on Windows: `http://127.0.0.1:7674/...` reaches Sophia in Ubuntu.
5. Install cloudflared in Ubuntu (the dashboard's Debian commands in step 6), not on Windows.
6. Windows stops Ubuntu some time after its last window closes, and with it Sophia. Keep an Ubuntu window open.

## 14. Troubleshooting

| You see | Why | What to do |
|---|---|---|
| `gardend: command not found` | The link from step 3 is missing | Run step 3's `ln` line again, or use `~/gardend/gardend` |
| macOS says gardend cannot be opened | The download flag (step 3) | `xattr -dr com.apple.quarantine ~/gardend` |
| `port 7674 on 127.0.0.1 is not free` | Sophia is already running (perhaps as the service), or another program uses the port | `gardend service status`; or pick another port with `--port` |
| The link opens an empty page, and gardend's ready message says `none found` beside `ui` | The `ui` folder is not beside the `gardend` program | Check `ls ~/gardend`; unpack the release again |
| "Cloudflare Access let you in, but this Sophia did not accept the sign-in", at `127.0.0.1` | Access is on (step 7), and gardend is 0.1.2 | Update gardend (step 10), or use `https://sophia.example.com` |
| The same message at your address | The team domain or AUD tag does not match | Check both in `~/.config/sophia/env`, then rerun `. ~/.config/sophia/env && gardend service install --profile ~/sophia` |
| `127.0.0.1` asks for your link, or says the key was not accepted | This browser has no key, or an old one | Open the link `gardend url --profile ~/sophia` prints |
| Changes do not save; "invalid origin" | `GARDEN_PUBLIC_BASE_URL` is missing, or differs from the address you typed | Fix it in `~/.config/sophia/env` and rerun the install line |
| Sophia opens at your address without asking for an email | No Access application covers that hostname | Fix 7.2 at once |
| Cloudflare error 1033, or the tunnel is not Healthy | cloudflared is not running, or the computer is asleep or offline | Wake the computer; check **Networking → Tunnels** |
| 502 Bad Gateway | gardend is not running | `gardend service status`, then read the log (step 5) |
| An upload fails with 413 | Cloudflare's free plan takes uploads up to 100 MB | Keep uploads under 100 MB (Sophia's files are limited to 50 MB anyway) |
| An agent's long request fails after about two minutes (524) | Cloudflare ends a request that is silent for 125 seconds | Ask the agent to use the job version of the tool, which answers at once, and to check on the job |
| sophia-mcp says Cloudflare Access refused | A wrong Client ID or Secret, or no **Service Auth** policy with that token | Check step 9 |
| An agent gets an HTML challenge page | Cloudflare's Bot Fight Mode is on | Turn it off for your domain (**Security → Bots**) |
| Chat says "Chat needs Node.js 22.19 or newer" | gardend found no `node`, or an old one, on its PATH | Section 11: install Node.js, then install the service again from a new Terminal |
| Chat replies with an error about an API key | No model key is set | `gardend chat key set --profile ~/sophia` (section 11) |
| No chat panel at all | The `services` folder is not beside the `gardend` program (gardend older than 0.1.4) | Update by replacing the whole `~/gardend` folder (step 10) |

## 15. Not included yet

- Agents that run programs inside Sophia come later. Your own agents can work with your gardens through sophia-mcp
  (step 9).
- Your chat history is not in the download.
- gardend on Windows itself (WSL2 works, section 13), and a service on Windows.
