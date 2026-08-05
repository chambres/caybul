Caybul for Linux (x86_64)

Contents
  caybul-gui   the app
  caybul       command-line version

Running
  chmod +x caybul-gui caybul   (if needed)
  ./caybul-gui

  The GUI needs GTK and a few X libraries present at runtime:
      sudo apt install libgtk-3-0 libxkbcommon0
  (package names vary by distro).

Using it
  Plug the two computers together (Thunderbolt/USB4 or Ethernet), open
  Caybul on both, and they connect on their own. Drag files in, press Send.
  Received files land in ~/Downloads/caybul-inbox.

  On Thunderbolt links, load the thunderbolt-net module if it isn't already:
      sudo modprobe thunderbolt-net
