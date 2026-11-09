# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
from gen import *
PA = lambda ms: {"op": "ext", "id": "pause", "text": str(ms)}
steps = [XD(), R("selectMailbox", accountRef="xd", text="inbox"), W(),
  R("selectMessage", text="xd phrase one"), W(), R("findSimilar"), W("sheet:findSimilar"), W(), P("findSimilarShown"),
  R("dismissFindSimilar"), W("noSheet:findSimilar"), PA(3000),
  R("findSimilar"), PA(1500), W(), P("findSimilarShown"), C("second-open-same-message-after-3s"),
  R("dismissFindSimilar"), PA(3000),
  R("selectMessage", text="xd phrase two"), W(), R("findSimilar"), PA(1500), W(), P("findSimilarShown"), C("open-other-message"),
  R("dismissFindSimilar"), PA(3000), R("findSimilar"), PA(1500), P("findSimilarShown"),
]
write("xd-e6", ["explore"], steps, d=".")
