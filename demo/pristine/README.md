# Demo project (pristine)

The source of truth for the demo workspace. `scripts/demo.sh` copies this into
`demo/workspace/`, which is gitignored, before every run.

The indirection exists because the demo agent really edits the file it is asked
to fix: without it the second run would start from an already-fixed `stats.js`
and the demo would have nothing to do.

The generated copy is what agents edit. `stats.js` has a real off-by-one in `median()` — for even-length input it
returns the upper of the two middle values instead of their average, so
`stats.test.js` fails until it is fixed. Ask an agent to fix it and you exercise
read, diff, permission and terminal in a single turn.
