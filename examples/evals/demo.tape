# Renders docs/evals.gif: the v0.4 demo against a real model.
#
# This makes billed API calls. From the repo root, with ANTHROPIC_API_KEY set
# in the shell you run vhs from. It uses the SDK from sdks/python/.venv if
# that exists, else whatever `python` is on PATH (see README.md):
#
#   cargo build -p tether-cli -p tetherd
#   vhs examples/evals/demo.tape
#
# The key is read from the environment and never typed, so it is not in the
# GIF. Recording the suite happens off screen, before the GIF starts; it takes
# a minute or two. For a GIF with no key and no cost, replace the hidden
# record command's `--model anthropic` with nothing and add
# `--scripts examples/evals/scripts` to the second `tether eval`.

Output docs/evals.gif
Set FontSize 13
Set Width 1300
Set Height 900

Hide
Type "export PATH=$PWD/target/debug:$PWD/sdks/python/.venv/bin:$PATH && python examples/evals/record.py --model anthropic >/dev/null && clear"
Enter
# Twenty tasks recorded against the real model.
Sleep 150s
Show

Type "tether eval examples/evals/suite | tail -3"
Enter
Sleep 6s

Type 'tether eval examples/evals/suite --system "$(cat examples/evals/regressed.txt)"'
Enter
# Twenty tasks asked again, live.
Sleep 150s
