import json, os, pathlib, sys, time

def read():
    return json.loads(sys.stdin.buffer.readline())
def write(value):
    print(json.dumps(value), flush=True)
init = read()
write({'jsonrpc':'2.0','id':init['id'],'result':{'protocol_version':'euler-managed-process/1'}})
read()
command = read()
pid = os.fork()
if pid == 0:
    time.sleep(1)
    pathlib.Path('descendant-after-return').write_text('alive')
    os._exit(0)
write({'jsonrpc':'2.0','id':command['id'],'result':{}})
shutdown = read()
write({'jsonrpc':'2.0','id':shutdown['id'],'result':{}})
read()
os._exit(1)
