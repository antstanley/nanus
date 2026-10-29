"""Frame-by-frame takeoff, posed and rendered through VectorCraft's MCP server.

The camera, branch, woodland and captions never move. Geometry is always posed
from the original anchors (not accumulated transforms), then exported at 24 fps.
The standalone animated SVG uses those same frames as discrete path keyframes.
"""
import argparse
import copy
import json
import math
from pathlib import Path
import subprocess
import xml.etree.ElementTree as ET

HERE = Path(__file__).resolve().parent
OUT = HERE / 'takeoff'
FPS = 24
COUNT = 144
SVG = 'http://www.w3.org/2000/svg'
ET.register_namespace('', SVG)


def smooth(a, b, t):
    u = max(0., min(1., (t-a)/(b-a)))
    return u*u*(3-2*u)


def mix(a, b, t):
    return a + (b-a)*t


def rotate(p, pivot, degrees, sx=1., sy=1.):
    x, y = (p[0]-pivot[0])*sx, (p[1]-pivot[1])*sy
    c, s = math.cos(math.radians(degrees)), math.sin(math.radians(degrees))
    return [pivot[0]+c*x-s*y, pivot[1]+s*x+c*y]


def flight(t):
    crouch = smooth(.9, 1.36, t)*(1-smooth(1.45, 1.74, t))
    lift = smooth(1.45, 1.83, t)
    air = max(0., t-1.8)
    dx = -35*lift - 140*air - 112*air*air
    dy = 22*crouch - 92*lift - 142*air - 23*air*air
    pitch = -7*crouch - 28*lift
    if t > 1.8:
        pitch += 2.5*math.sin(2*math.pi*2.8*air)*smooth(1.8,2.1,t)
        dy += 3*math.sin(2*math.pi*2.8*air)
    return crouch, lift, dx, dy, pitch


def body(p, t):
    crouch, lift, dx, dy, pitch = flight(t)
    q = rotate(p, (472,490), pitch, 1+.025*crouch, 1-.09*crouch)
    breath = 1.2*math.sin(2*math.pi*t/.9)*(1-smooth(.8,1.,t))
    return [q[0]+dx, q[1]+dy+breath]


def wing(p, t, far=False):
    deploy = smooth(1.10, 1.72, t)
    # Start the first upstroke during the crouch; then repeat power strokes.
    if t < 1.72:
        angle = -118*smooth(1.1,1.72,t)
    else:
        phase = 2*math.pi*2.8*(t-1.72)
        angle = -45 - 73*math.cos(phase + (.25*smooth(1.72,1.9,t) if far else 0))
    if far:
        angle -= 12*deploy
    root = (457,417)
    # Longitudinal and transverse wing coordinates: deploy the span and fan.
    vx, vy = .749, .663
    x, y = p[0]-root[0], p[1]-root[1]
    u, v = vx*x+vy*y, -vy*x+vx*y
    v *= 1+deploy*(1.05 + .35*max(0,u)/220)
    u *= 1+.48*deploy
    if far:
        u *= .88
        v *= .72
    q = [root[0]+vx*u-vy*v, root[1]+vy*u+vx*v]
    q = rotate(q, root, angle)
    if far:
        q[0] += 18*deploy
        q[1] -= 6*deploy
    return body(q,t)


def posed(p, part, t):
    if part in ('wing','far-wing'):
        return wing(p,t,part=='far-wing')
    if part == 'tail':
        fan = smooth(1.35,1.8,t)
        q = rotate(p, (555,513), -15*fan, 1., 1.+.08*fan)
        return body(q,t)
    if part == 'head':
        # Small anticipatory look in the direction of departure.
        look = smooth(.65,1.2,t)*(1-smooth(1.5,1.85,t))
        level = 22*smooth(1.45,1.83,t)
        return body(rotate(p,(407,379),7*look+level),t)
    if part in ('near-leg','far-leg'):
        pivot = (462,574) if part=='near-leg' else (512,563)
        tuck = smooth(1.72,2.16,t)
        q = rotate(p,pivot,-38*tuck,1.,1.-.40*tuck)
        b = body(q,t)
        # Keep toes planted until the push has actually released the branch.
        planted = 1-smooth(1.50,1.67,t)
        weight = smooth(576,633,p[1])*planted
        return [mix(b[0],p[0],weight), mix(b[1],p[1],weight)]
    return body(p,t)


def pose_subpaths(node, part, t):
    out = []
    for sub in node['kind']['path']['subpaths']:
        anchors = []
        for a in sub['anchors']:
            x,y = posed(a['p'],part,t)
            anchor = {'x':round(x,4),'y':round(y,4)}
            for k in ('in','out'):
                if k in a:
                    anchor[k] = [round(v,4) for v in posed(a[k],part,t)]
            anchors.append(anchor)
        out.append({'anchors':anchors,'closed':sub.get('closed',False)})
    return out


def path_data(subpaths):
    commands = []
    def pt(p):
        return f'{p[0]:.3f} {p[1]:.3f}'
    for sub in subpaths:
        aa = sub['anchors']
        commands.append('M'+pt([aa[0]['x'],aa[0]['y']]))
        pairs = list(zip(aa,aa[1:]))
        if sub['closed']:
            pairs.append((aa[-1],aa[0]))
        for a,b in pairs:
            pa,pb = [a['x'],a['y']],[b['x'],b['y']]
            if 'out' in a or 'in' in b:
                commands.append('C'+pt(a.get('out',pa))+' '+pt(b.get('in',pb))+' '+pt(pb))
            else:
                commands.append('L'+pt(pb))
        if sub['closed']:
            commands.append('Z')
    return ' '.join(commands)


class MCP:
    def __init__(self):
        self.proc = subprocess.Popen(['vectorcraft-cli','mcp','--headless',
            '--automation-read-root',str(HERE),'--automation-write-root',str(HERE)],
            stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True)
        self.seq = 0
        self.log = []
        self.request('initialize',{'protocolVersion':'2025-06-18','capabilities':{},
            'clientInfo':{'name':'nanus-frame-animation','version':'1'}})
        self.proc.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
        self.proc.stdin.flush()

    def request(self,method,params):
        self.seq += 1
        self.proc.stdin.write(json.dumps({'jsonrpc':'2.0','id':self.seq,'method':method,'params':params})+'\n')
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError('MCP server closed unexpectedly')
            result = json.loads(line)
            if result.get('id')==self.seq:
                break
        if 'error' in result:
            raise RuntimeError(result)
        result = result['result']
        if result.get('isError'):
            raise RuntimeError(result)
        return result

    def call(self,tool_name,**arguments):
        result = self.request('tools/call',{'name':tool_name,'arguments':arguments})
        blocks = [c['text'] for c in result.get('content',[]) if c['type']=='text']
        data = json.loads(blocks[0]) if blocks else {}
        if isinstance(data,dict) and data.get('failed',0):
            raise RuntimeError(data)
        self.log.append({'tool':tool_name,'frame':getattr(self,'frame',None),
            'completed':data.get('completed'),'warnings':data.get('warnings')})
        return data

    def run(self,command,**params):
        return self.call('run_command',command=command,params=params)

    def close(self):
        (OUT/'mcp-animation-log.json').write_text(json.dumps(self.log,indent=2))
        self.proc.stdin.close()
        self.proc.wait()


def create_animated_svg(original, models, parts, far_models):
    root = ET.parse(original).getroot()
    root.set('overflow','hidden')
    root.set('role','img')
    root.set('aria-labelledby','animation-title animation-description')
    title = root.find(f'{{{SVG}}}title')
    title.set('id','animation-title')
    title.text = 'Dwarf jay taking flight'
    desc = ET.SubElement(root,f'{{{SVG}}}desc',{'id':'animation-description'})
    desc.text = ('Six-second looping, 24-frame-per-second takeoff. The jay crouches, '
        'releases its grip and flaps toward the upper left, fully leaving the page. '
        'The camera, scale, branch, woodland and typography remain fixed. '
        'Every keyframe was posed and rendered using VectorCraft MCP.')
    by_name = {n.get('data-name', n.get('id')):n for n in root.iter()
               if n.get('data-name') or n.get('id')}
    # Add the far wing between the tail layer and body, exactly as in MCP.
    far_group = ET.Element(f'{{{SVG}}}g',{'id':'far-wing','data-name':'Far wing','opacity':'0'})
    body_layer = by_name['04 - Body and pale throat']
    root.insert(list(root).index(body_layer),far_group)
    for node in far_models:
        ET.SubElement(far_group,f'{{{SVG}}}path',{'data-name':node['name'],
            'fill':node['svg_fill'],'stroke':'none'})
    by_name.update({n.get('data-name'):n for n in far_group})
    times = ';'.join(f'{i/COUNT:.7f}' for i in range(COUNT+1))
    for node in models+far_models:
        element = by_name[node['name']]
        element.attrib.pop('transform',None)
        part = parts[node['id']]
        data = [path_data(pose_subpaths(node,part,i/FPS)) for i in range(COUNT)]
        data.append(data[-1])
        if element.tag != f'{{{SVG}}}path':
            # Eye ellipses become equivalent cubic paths, preserving their paint.
            element.tag=f'{{{SVG}}}path'
            for key in ('cx','cy','r','rx','ry','x','y','width','height'):
                element.attrib.pop(key,None)
        element.set('d',data[0])
        ET.SubElement(element,f'{{{SVG}}}animate',{'attributeName':'d',
            'dur':'6s','repeatCount':'indefinite','calcMode':'discrete',
            'keyTimes':times,'values':';'.join(data)})
    opacity = [f'{smooth(1.1,1.72,i/FPS):.4f}' for i in range(COUNT)]
    opacity.append(opacity[-1])
    ET.SubElement(far_group,f'{{{SVG}}}animate',{'attributeName':'opacity',
        'dur':'6s','repeatCount':'indefinite','calcMode':'discrete',
        'keyTimes':times,'values':';'.join(opacity)})
    ET.indent(root,space='  ')
    ET.ElementTree(root).write(OUT/'dwarf-jay-takeoff.svg',encoding='utf-8',xml_declaration=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--samples',action='store_true',help='Render only contact-sheet sample frames')
    args=parser.parse_args()
    OUT.mkdir(exist_ok=True)
    (OUT/'frames').mkdir(exist_ok=True)
    (OUT/'svg-frames').mkdir(exist_ok=True)
    client=MCP()
    try:
        client.call('open_file',path=str(HERE/'dwarf-jay.vectorcraft'))
        doc=client.run('document.json')
        (OUT/'source-model.json').write_text(json.dumps(doc))
        # Summary ids let the model's geometry be found without assuming format layout.
        summary=client.call('inspect_document',depth=1)
        wanted={}
        for layer in summary['layers']:
            label=layer['name'][:2]
            for obj in layer.get('children',[]):
                name=obj['name']
                if label=='03':
                    wanted[obj['id']]='far-leg' if name in ('Far leg','Far toes') else 'tail'
                elif label=='04': wanted[obj['id']]='body'
                elif label=='05': wanted[obj['id']]='wing'
                elif label=='06': wanted[obj['id']]='head'
                elif label=='07': wanted[obj['id']]='near-leg'
        models=[]
        for id in sorted(wanted):
            models.append(client.run('document.node',id=id))
        parts=dict(wanted)
        client.run('layer.setCurrent',id=36)
        far_layer=client.run('layer.new',name='03b - Far wing in flight')['id']
        far_models=[]
        for node in models:
            if parts[node['id']]!='wing':
                continue
            # Only solid filled areas are needed for the more distant wing.
            fill=node.get('appearance',{}).get('items',[{}])[0].get('paint',{})
            if fill.get('type')!='solid':
                continue
            color=fill['color']
            rgb=[round(color[c]*255*.82) for c in ('r','g','b')]
            hexcolor='#'+''.join(f'{v:02x}' for v in rgb)
            d=path_data(pose_subpaths(node,'wing',0))
            result=client.call('draw_path',name='Far '+node['name'],d=d,fill=hexcolor,stroke='none')
            far=copy.deepcopy(node)
            far['id']=result['id']
            far['name']='Far '+node['name']
            far['svg_fill']=hexcolor
            far_models.append(far)
            parts[far['id']]='far-wing'
        create_animated_svg(HERE/'dwarf-jay.svg',models,parts,far_models)
        samples=[0,27,34,40,44,48,52,57,64,72,88,120]
        indices=samples if args.samples else range(COUNT)
        for i in indices:
            client.frame=i
            t=i/FPS
            steps=[]
            for node in models+far_models:
                steps.append({'id':'path.setAnchors','params':{'id':node['id'],
                    'subpaths':pose_subpaths(node,parts[node['id']],t)}})
            steps.append({'id':'layer.setProps','params':{'id':far_layer,
                'visible':True}})
            # Layer opacity is not a layer property; target its child objects.
            steps.append({'id':'object.setProps','params':{'ids':[n['id'] for n in far_models],
                'opacity':round(100*smooth(1.1,1.72,t),4)}})
            client.call('command_batch',steps=steps,atomic=True,label=f'Takeoff frame {i:03}')
            client.call('export',path=str(OUT/'frames'/f'frame-{i:03}.png'),format='png',scale=1)
            client.call('export',path=str(OUT/'svg-frames'/f'frame-{i:03}.svg'),format='svg',outlineText=True,options={'decimals':3})
            if i in (0,44):
                client.call('save_file',path=str(OUT/f'pose-{i:03}.vectorcraft'))
            if i%12==0 or args.samples:
                print(f'Rendered {i:03}/{COUNT} at {t:.3f}s',flush=True)
            client.run('edit.clearHistory')
        print('All requested frames rendered through MCP.',flush=True)
    finally:
        client.close()

if __name__=='__main__':
    main()
