Caybul for macOS (universal — Apple Silicon and Intel)

Contents
  Caybul.app   the app
  caybul       command-line version (optional)

First launch
  The app isn't notarized yet, so macOS blocks a plain double-click.
  Right-click Caybul.app, choose Open, then Open again. After that it
  opens normally. Same for the CLI if Terminal refuses it:
      xattr -dr com.apple.quarantine caybul

Using it
  Plug the two computers together (Thunderbolt/USB4 or Ethernet), open
  Caybul on both, and they connect on their own. Drag files in, press Send.
  Received files land in Downloads/caybul-inbox.

  If macOS asks to allow local network access, allow it.
