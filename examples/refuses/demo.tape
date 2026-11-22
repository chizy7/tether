# Renders docs/media/refuses.gif. From the repo root:
#
#   cargo build -p tether-cli
#   vhs examples/refuses/demo.tape

Output docs/media/refuses.gif
Set FontSize 14
Set Width 1400
Set Height 760

# Off camera: put the built binary on PATH and start from a clean example.
Hide
Type "export PATH=$PWD/target/debug:$PATH && cd examples/refuses && rm -rf .tether && clear"
Enter
Show

Type "tether run --script script.json --policy tether.toml"
Enter
Sleep 3s
Type "tether inspect .tether/run_*.jsonl"
Enter
Sleep 7s
Type "clear"
Enter
Type "tether inspect .tether/run_*.jsonl | grep -B1 -A1 'rule no-rm'"
Enter
Sleep 7s
