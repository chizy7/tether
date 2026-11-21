# Renders docs/replays.gif: the v0.3 demo against a real model.
#
# This makes billed API calls. From the repo root, with ANTHROPIC_API_KEY set
# in the shell you run vhs from. It uses the SDK from sdks/python/.venv if
# that exists, else whatever `python` is on PATH (see README.md):
#
#   cargo build -p tether-cli -p tetherd
#   vhs examples/replays/demo.tape
#
# The key is read from the environment and never typed, so it is not in the GIF.

Output docs/replays.gif
Set FontSize 13
Set Width 1500
Set Height 780

Hide
Type "export PATH=$PWD/target/debug:$PWD/sdks/python/.venv/bin:$PATH && clear"
Enter
Show

Type "python examples/replays/demo.py --model anthropic"
Enter
# Twenty tasks, each recorded, replayed and run again live.
Sleep 240s
