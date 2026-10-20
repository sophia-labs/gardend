# Sophia (the app)

Self-hosted Sophia in a native window, with everything it runs inside it: the `gardend` engine, the Shrubbery UI, chat
(the Vehicle) and Sophia Compute (Choreograph), and the Node.js those two run on. Open it and it is Sophia on this Mac,
as `gardend serve` gives it, on http://127.0.0.1:7674 (a link opened in a browser works too while the app runs).

- **This Mac** (the default): the app starts `gardend serve` and opens its link. Data lives in gardend's own default
  folder, `~/Library/Application Support/Sophia`, unless the one line of
  `~/Library/Application Support/com.sophia-labs.sophia/profile` names another (`~/sophia`, say; an unpacked "Download
  my gardens" folder is a profile). Quitting the app stops gardend after its final flush. If a gardend already serves
  that folder (a `gardend service`, say), the app opens that one and leaves it running.
- **Hosted**: garden.sophia-labs.com, with its own sign-in. Set `SOPHIA_URL` to open another hosted address.

**Place** has Show Local Data in Finder and Show Log (`~/Library/Logs/Sophia/app.log`; lines carrying the key are not
kept). Links to other sites open in your browser; downloads go to ~/Downloads. **Go** has Back, Forward, Home, Reload and
"Open This Page in Browser". Right-click → Inspect Element opens the Web Inspector.

Chat needs a model key: `gardend chat key set --profile <folder>` (the app's `Contents/MacOS/gardend` will do), or the
chat panel's own prompt. Sophia Compute is off until you enable it: `gardend service enable choreograph --profile
<folder>`. Reading scans needs Tesseract on this Mac (`brew install tesseract`); pdfium is in the app.

## Build

With the buildbox (`scripts/buildbox`, one warm build of gardend per Garden line) and checkouts of the other three:

```sh
scripts/buildbox app --install --shrubbery ../shrubbery --vehicle ../vehicle --choreograph ../choreograph
scripts/buildbox app --only ui --shrubbery ../shrubbery        # just the UI, into the installed app: then Cmd-R
scripts/buildbox app --only services --vehicle ../vehicle --choreograph ../choreograph   # then quit and reopen
```

(`BUILDBOX_SHRUBBERY`, `BUILDBOX_VEHICLE` and `BUILDBOX_CHOREOGRAPH` save typing the paths.) `shell/build-app.sh --help`
says what goes where. The UI and the engine must come from the same moment: a Shrubbery newer than its gardend can fail
to sync.

**Mac users:** the app is not notarized. The first time, right-click Sophia.app → Open → Open (or run
`xattr -dr com.apple.quarantine ~/Applications/Sophia.app`).

Overrides: `SOPHIA_URL` (hosted address), `SOPHIA_GARDEND` (engine binary), `SOPHIA_LOCAL_PROFILE` (local data folder),
`SOPHIA_LOCAL_PORT` (another port than 7674, to try the app beside a gardend already serving there).
