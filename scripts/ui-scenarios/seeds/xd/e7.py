# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from gen import *
PA = lambda ms: {"op": "ext", "id": "pause", "text": str(ms)}
steps = [XD(), R("selectMailbox", accountRef="xd", text="inbox"), W(),
  R("selectMessage", text="xd phrase one"), W(), R("findSimilar"), W("sheet:findSimilar"), W(), P("findSimilarShown"),
  R("dismissFindSimilar"), W("noSheet:findSimilar"), PA(3000),
  R("findSimilar"), P("findSimilarShown"), PA(100), P("findSimilarShown"), PA(400), P("findSimilarShown"), C("second-open"), PA(1500), P("findSimilarShown"),
  {"op": "assert", "expect": [{"sheetAttached": True}]},
]
write("xd-e7", ["explore"], steps, d=".")
