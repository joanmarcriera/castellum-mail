#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Stack capture PNGs vertically into one sheet for review. Usage: sheet.py out.png in1.png in2.png ..."""
import sys
from PIL import Image
def load(p):
    i=Image.open(p).convert("RGBA"); b=Image.new("RGBA",i.size,(255,255,255,255)); b.alpha_composite(i)
    b=b.convert("RGB")
    # crop: drop the (uncaptured) sidebar strip to keep sheets small
    return b.crop((int(sys.argv[1].split("@")[1]) if "@" in sys.argv[1] else 0,0,b.width,b.height))
ims=[load(p) for p in sys.argv[2:]]
w=max(i.width for i in ims); h=sum(i.height for i in ims)+4*len(ims)
o=Image.new("RGB",(w,h),(255,0,255)); y=0
for i in ims: o.paste(i,(0,y)); y+=i.height+4
o.save(sys.argv[1].split("@")[0])
