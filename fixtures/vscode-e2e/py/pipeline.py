# FROZEN FIXTURE - do not reformat. expected.json pins line numbers.
# Case: second language. `format` here must not merge with the two
# TypeScript `format` definitions in ts/util.ts and ts/report.ts.


def format(rows):
    return ",".join(rows)


def run_pipeline(rows):
    return format(rows)
