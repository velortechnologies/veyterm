"""Exercise the real veyterm binary against a disposable pinned-TLS Gateway double."""
import base64, hashlib, json, os, pathlib, pty, select, socket, ssl, struct, subprocess, tempfile, threading, time, fcntl, termios
with tempfile.TemporaryDirectory(prefix='veyterm-wss-fault-') as tmp:
    cert=tmp+'/cert.pem'; private=tmp+'/key.pem'
    subprocess.run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-keyout',private,'-out',cert,'-days','1','-subj','/CN=localhost'],check=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    der=ssl.PEM_cert_to_DER_cert(pathlib.Path(cert).read_text()); pin=hashlib.sha256(der).hexdigest()
    context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);context.load_cert_chain(cert,private)
    listener=socket.socket();listener.bind(('127.0.0.1',0));listener.listen()
    port=listener.getsockname()[1]; events=[]; first=threading.Event(); recovered=threading.Event(); answered=threading.Event(); failures=[]
    def exact(s,n):
        b=b''
        while len(b)<n:
            c=s.recv(n-len(b))
            if not c: raise EOFError()
            b+=c
        return b
    def frame(s,opcode,payload):
        h=bytes([128|opcode]);n=len(payload)
        h+=bytes([n]) if n<126 else bytes([126])+struct.pack('!H',n)
        s.sendall(h+payload)
    def send(s,v):frame(s,1,json.dumps(v).encode())
    def receive(s):
        a,b=exact(s,2); n=b&127
        if n==126:n=struct.unpack('!H',exact(s,2))[0]
        if n==127:n=struct.unpack('!Q',exact(s,8))[0]
        mask=exact(s,4) if b&128 else None;data=exact(s,n)
        if mask:data=bytes(c^mask[i%4] for i,c in enumerate(data))
        return a&15,data
    messages=[]
    def serve():
        try:
            for connection in [1,2]:
                raw,_=listener.accept();s=context.wrap_socket(raw,server_side=True);s.settimeout(35)
                headers=b''
                while not headers.endswith(b'\r\n\r\n'):headers+=exact(s,1)
                key=next(line.split(b':',1)[1].strip() for line in headers.split(b'\r\n') if line.lower().startswith(b'sec-websocket-key:'))
                accept=base64.b64encode(hashlib.sha1(key+b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest())
                s.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: '+accept+b'\r\n\r\n')
                send(s,{'type':'event','event':'connect.challenge','payload':{'nonce':'local-fault-test','ts':int(time.time()*1000)}})
                try:
                    while True:
                        op,data=receive(s)
                        if op==8:break
                        if op==9:
                            events.append((connection,'ping'))
                            if connection==2:frame(s,10,data)
                            continue
                        if op!=1:continue
                        r=json.loads(data);method=r['method'];params=r.get('params',{});events.append((connection,method))
                        if method=='connect':result={'server':{'version':'test'},'features':{'methods':['sessions.list']}}
                        elif method=='sessions.list':result={'sessions':[{'key':'veyterm-fault-test','displayName':'veyterm fault test'}]}
                        elif method=='chat.history':
                            result={'messages':list(messages)}
                            (first if connection==1 else recovered).set()
                        elif method=='commands.list':result={'commands':[]}
                        elif method=='models.list':result={'models':[]}
                        elif method=='chat.send':
                            messages.append({'role':'user','content':params['message']})
                            events.append((connection,'message:'+params['message']))
                            if connection==1:continue # accept, but deliberately lose acknowledgment
                            result={'runId':params['idempotencyKey'],'status':'started'}
                        else:result={}
                        send(s,{'type':'res','id':r['id'],'ok':True,'payload':result})
                        if method=='chat.send':
                            messages.append({'role':'assistant','content':'SECOND-IN-ORDER'})
                            send(s,{'type':'event','event':'chat','payload':{'sessionKey':'veyterm-fault-test','runId':params['idempotencyKey'],'state':'final','message':messages[-1]}})
                            answered.set()
                except EOFError:pass
                finally:s.close()
        except Exception as e:failures.append(type(e).__name__+': '+str(e))
    threading.Thread(target=serve,daemon=True).start()
    master,slave=pty.openpty();fcntl.ioctl(slave,termios.TIOCSWINSZ,struct.pack('HHHH',30,100,0,0))
    process=subprocess.Popen([os.environ.get('VEYTERM_TEST_BINARY', str(pathlib.Path(__file__).resolve().parents[1] / 'target/release/veyterm')),f'wss://127.0.0.1:{port}','--tls-sha256',pin,'--session','veyterm-fault-test'],stdin=slave,stdout=slave,stderr=slave,env=dict(os.environ,VEYTERM_TOKEN='local-dummy-token',TERM='xterm-256color'));os.close(slave)
    stop=threading.Event();terminal=bytearray()
    def drain():
        while not stop.is_set():
            try:
                if select.select([master],[],[],.1)[0]:terminal.extend(os.read(master,65536))
            except OSError:return
    threading.Thread(target=drain,daemon=True).start()
    try:
        assert first.wait(10),('initial history missing',failures)
        time.sleep(.3);os.write(master,b'FIRST\r')
        assert recovered.wait(30),('heartbeat did not recover',failures,events)
        time.sleep(.3);os.write(master,b'SECOND\r')
        assert answered.wait(10),('post-reconnect send failed',failures,events)
        time.sleep(1);os.write(master,b'\x11');process.wait(timeout=5)
        assert sum(e[1]=='message:FIRST' for e in events)==1,events
        assert sum(e[1]=='message:SECOND' for e in events)==1,events
        assert process.returncode==0,process.returncode
        assert not failures,failures
        print(json.dumps({'result':'PASS','connections':2,'first_send_count':1,'second_send_count':1,'first_connection_pongs':'withheld','accepted_first_send_ack':'withheld','auto_replay':False,'new_message_after_reconnect':'answered','real_binary_and_pty':True}))
    finally:
        if process.poll() is None:process.terminate();process.wait(timeout=5)
        stop.set();os.close(master);listener.close()
