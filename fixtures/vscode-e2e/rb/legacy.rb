# FROZEN FIXTURE - do not reformat. expected.json pins line numbers.
# Case: Ruby has a Phase A grammar but no Phase B provider installed by
# default. find_references on `legacy_export` must return the softened
# partial-coverage message (#450 / #551), not a definitive zero.

def legacy_export(payload)
  payload.to_s
end
