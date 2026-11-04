import json,subprocess,sys
d=json.load(open('hosts.json'))['public']
r=subprocess.run(['ssh','-o','BatchMode=yes',f"admin@{d['node'+sys.argv[1]]}",'cat /data/timing'],capture_output=True,text=True)
exp=int(sys.argv[2]); out=r.stdout.strip()
import re
m=re.search(r'n=(\d+)',out); n=int(m.group(1)) if m else -1
print('   '+('COUNT OK' if n==exp else f'INVALID count {n}, expected {exp}'))
print('\n'.join('   '+l[:125] for l in out.splitlines()))
