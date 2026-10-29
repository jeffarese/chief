# VHS tape for the README demo; run it through scripts/demo/demo.sh, which builds the world it
# needs (fake claude and gh, a temp HOME) and moves the results to docs/.
Output demo.gif
Output demo.mp4

Set Shell "zsh"
Set FontFamily "SF Mono, Menlo, monospace"
Set FontSize 15
Set LineHeight 1.15
Set Width 1480
Set Height 860
Set Padding 18
Set Framerate 30
Set Theme "Catppuccin Mocha"
Set WindowBar Colorful
Set BorderRadius 10
Set TypingSpeed 40ms
Set WaitTimeout 75s

Hide
Type `source ./env && exec tmux -L chief-demo -f "$DEMO/tmux.conf" new-session`
Enter
Sleep 1.5s
Type "clear"
Enter
Sleep 500ms
Show

# A new project: the guided setup.
Sleep 1s
Type "chief"
Sleep 500ms
Enter
Sleep 1.8s
Type "Payments API"
Sleep 500ms
Tab
Type "Ship rate limiting without breaking checkout"
Sleep 900ms
Enter
Sleep 1.6s
Enter
Sleep 1.6s
Enter
Sleep 2.5s
Enter

# The conversation with the coordinator on the left, the board on the right.
Wait+Screen /coordinator of Payments API/
Sleep 1.8s
Type "add rate limiting to the API, fix the flaky checkout test, and speed up search"
Sleep 700ms
Enter
Wait+Screen /needs you/
Wait+Screen /Waiting on you · 1/
Sleep 1.5s

# On the board: the thread that waits on you, opened in the left pane.
Ctrl+B
Right
Sleep 400ms
Type "g"
Sleep 400ms
Type "j"
Sleep 1s
Enter
Wait+Screen /Enter to select/
Sleep 2s

# Answer its question there.
Ctrl+B
Left
Down
Sleep 500ms
Up
Sleep 700ms
Enter
Wait+Screen /Writing tests/
Sleep 3s

# Back to the coordinator, which reports the pull request.
Ctrl+B
Right
Sleep 300ms
Escape
Wait+Screen /up for review/
Sleep 5s
