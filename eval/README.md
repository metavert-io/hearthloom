# Eval suites

`hearthloom eval` measures how well Hearthloom answers questions about
**your** life. The questions and their expected answers are personal data,
so they live in your data home, never in this repository:

```
~/.hearthloom/eval/personal-eval.yaml   # retrieval checks (no LLM tokens)
~/.hearthloom/eval/ask-eval.yaml        # full Ask engine (spends tokens)
~/.hearthloom/eval/<name>.yaml          # any other set, e.g. a private one
```

`hearthloom init` (and the first `hearthloom eval`) copies the invented
examples in `examples/` there for you to edit. Then:

```
hearthloom eval                     # personal-eval.yaml
hearthloom eval --ask               # ask-eval.yaml (real tokens, prints the bill)
hearthloom eval --ask --only id1    # a subset
hearthloom eval --suite <name>      # ~/.hearthloom/eval/<name>.yaml
hearthloom eval path/to/file.yaml   # any file
```

Cases marked `skip:` are templates waiting for your own facts; they are
reported and never run. Run the Ask suite before and after any model,
prompt or Ask-loop change and compare pass count, rounds, cost and time.
