# Renders docs/media/demo.gif. From the repo root:
#
#   cargo build -p tether-cli
#   cargo build -p tether-core --example raw-loop
#   rm -rf target/raw-loop
#   vhs demo.tape

Output docs/media/demo.gif
Set FontSize 14
Set Width 1400
Set Height 700

Type "./target/debug/examples/raw-loop"
Enter
Sleep 2s
Type "./target/debug/tether inspect target/raw-loop/run_*.jsonl"
Enter
Sleep 5s
Type "./target/debug/tether verify target/raw-loop/run_*.jsonl"
Enter
Sleep 3s
