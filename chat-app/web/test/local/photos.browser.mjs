import assert from 'node:assert/strict';
import {mkdirSync,readFileSync,realpathSync,truncateSync,unlinkSync,writeFileSync} from 'node:fs';
import {dirname,join,sep} from 'node:path';

// Synthetic APP1: Orientation=6 and a GPS IFD with synthetic coordinates.
function withGps(jpeg) {
  const tiff=Buffer.alloc(140);let at=0;
  const u16=n=>{tiff.writeUInt16LE(n,at);at+=2;}, u32=n=>{tiff.writeUInt32LE(n,at);at+=4;};
  const field=(tag,type,count,value)=>{u16(tag);u16(type);u32(count);u32(value);};
  u16(0x4949);u16(42);u32(8);
  u16(2);field(0x0112,3,1,6);field(0x8825,4,1,38);u32(0);
  u16(4);field(1,2,2,0x4e);field(2,5,3,92);field(3,2,2,0x45);field(4,5,3,116);u32(0);
  for(const n of [12,1,30,1,0,1,34,1,15,1,0,1])u32(n);
  assert.equal(at,tiff.length);
  const app=Buffer.from([0xff,0xe1,0,tiff.length+8,69,120,105,102,0,0]);
  return Buffer.concat([jpeg.subarray(0,2),app,tiff,jpeg.subarray(2)]);
}

// A valid blank one-page PDF, including the byte offsets for its xref table.
function tinyPdf() {
  let body='%PDF-1.4\n';
  const offsets=['0000000000 65535 f \n'];
  ['<< /Type /Catalog /Pages 2 0 R >>','<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>'].forEach((object,i)=>{
    offsets.push(`${String(body.length).padStart(10,'0')} 00000 n \n`);
    body+=`${i+1} 0 obj\n${object}\nendobj\n`;
  });
  return Buffer.from(`${body}xref\n0 4\n${offsets.join('')}trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n${body.length}\n%%EOF\n`);
}

async function agentFiles(page,rig,direct,dom,shot) {
  const root=rig.serveRoot;
  const report=join(root,'build report.md'), image=join(root,'agent-shot.png'), large=join(root,'large.png');
  const local=join(dirname(root),'local-only.log'), gone=join(root,'removed.md'), oversized=join(root,'oversized.log');
  const text='# Build report\n\n3 checks passed.\n\n<b>Literal markup</b>\n';
  writeFileSync(report,text);
  const screenshot=Buffer.from(await page.evaluate(()=>{
    const canvas=document.createElement('canvas');canvas.width=360;canvas.height=200;
    const ctx=canvas.getContext('2d');ctx.fillStyle='#202c39';ctx.fillRect(0,0,360,200);
    ctx.fillStyle='#77d5ad';ctx.font='24px monospace';ctx.fillText('Build complete',30,75);
    ctx.fillStyle='#e7e8ee';ctx.font='18px monospace';ctx.fillText('3 checks passed',30,122);
    return canvas.toDataURL('image/png').split(',')[1];
  }),'base64');
  writeFileSync(image,screenshot);writeFileSync(large,screenshot);truncateSync(large,1024*1024+1);
  writeFileSync(local,'Private fixture');writeFileSync(oversized,'Oversized fixture');truncateSync(oversized,32*1024*1024+1);
  await page.locator('#thread-back').click();
  const url=page.url(), fileGets=[];
  page.on('request',req=>{
    const path=new URL(req.url());
    if(req.method()==='GET'&&path.pathname==='/file') fileGets.push(path.searchParams);
  });
  const post=rig.json('lead','post','--to','owner','--body','Build artifacts',
    ...[report,image,local,gone,large,oversized].flatMap(path=>['--attach',path]));
  assert.equal(post.channel,direct);
  const row=`#posts [data-post-id="${post.post_id}"]`, message=page.locator(row);
  const viewer=page.getByRole('dialog'), close=()=>viewer.getByRole('button',{name:'Close'}).click();
  await Promise.all(['local-only.logOn the computer','removed.mdNo longer there','oversized.logOn the computer']
    .map(text=>dom(page,`${row} .file-chip`,text)));
  const chips=await message.locator('.file-chip').allTextContents();
  assert.ok(chips.includes('local-only.logOn the computer only'));
  assert.ok(chips.includes('oversized.logOn the computer only'));
  assert.ok(chips.includes('removed.mdNo longer there'));
  const download=message.getByRole('link',{name:/^large\.png/});
  assert.equal(await download.getAttribute('download'),'large.png');
  for(const name of [/local-only\.log/,/oversized\.log/,/removed\.md/]) {
    assert.equal(await message.getByRole('link',{name}).count(),0);
    assert.equal(await message.getByRole('button',{name,disabled:false}).count(),0);
  }
  assert.ok(!fileGets.some(query=>query.get('p')===report),'HEAD does not preload the report');
  assert.ok(fileGets.some(query=>query.get('p')===large&&query.get('thumb')==='1'));
  assert.ok(!fileGets.some(query=>query.get('p')===large&&!query.has('thumb')),'oversized image does not load full bytes');
  const open=message.getByRole('button',{name:/^build report\.md/});
  await open.click();
  const preview=viewer.locator('#file-markdown');
  await dom(page,'#photo-view[open] #file-markdown strong','Build report');
  assert.deepEqual(await preview.locator('strong').allTextContents(),['Build report']);
  assert.ok((await preview.textContent()).includes('<b>Literal markup</b>'));
  assert.equal(await preview.locator('b').count(),0,'raw tags are literal');
  await shot('agent-report');await close();await viewer.waitFor({state:'hidden'});
  const thumb=message.getByRole('button',{name:'agent-shot.png',exact:true}).locator('img');
  await thumb.scrollIntoViewIfNeeded();
  assert.equal(await thumb.getAttribute('loading'),'lazy');
  const dimensions=img=>img.decode().then(()=>[img.naturalWidth,img.naturalHeight]);
  assert.deepEqual(await thumb.evaluate(dimensions),[360,200]);
  await thumb.click();
  assert.deepEqual(await viewer.locator('img').evaluate(dimensions),[360,200]);
  assert.ok(fileGets.filter(query=>query.get('p')===image).every(query=>query.get('thumb')==='1'));
  await shot('agent-preview');await close();await viewer.waitFor({state:'hidden'});
  unlinkSync(report); // A report may change between HEAD and GET.
  await open.click();
  const retry=viewer.getByRole('link',{name:'Download'});
  await retry.waitFor({state:'visible'});
  assert.equal(await retry.getAttribute('download'),'build report.md');
  assert.equal(await viewer.locator('pre:visible').textContent(),"Couldn't preview this file.");
  assert.equal(await preview.textContent(),'','the previous preview did not survive a failed reopen');
  await close();assert.equal(page.url(),url);
  console.log('real agent files: bounded Markdown reader and lazy PNG; unavailable and size-limited chips; no navigation');
}

export async function photosBrowser(browser,rig,wire,{direct,directRoot},dom) {
  const context=await browser.newContext({viewport:{width:412,height:915},deviceScaleFactor:1,hasTouch:true,acceptDownloads:true});
  const page=await context.newPage();
  const shots=process.env.ATM_CHAT_TEST_SHOTS;
  const shot=async name=>{if(shots){mkdirSync(shots,{recursive:true});await page.screenshot({path:join(shots,`${name}.png`)});}};
  const plus=kind=>page.locator(`#${kind}-form .attach-plus`);
  const choice=(kind,name)=>page.locator(`#${kind}-form`).getByRole('button',{name,exact:true});
  const choose=async(kind,name,files)=>{
    await plus(kind).click();
    assert.equal(await choice(kind,name).count(),1,`+ offers ${name}`);
    const [chooser]=await Promise.all([page.waitForEvent('filechooser'),choice(kind,name).click()]);
    assert.ok(chooser.isMultiple());
    assert.equal(await chooser.element().getAttribute('accept'),name==='Photos'?'image/*':null);
    await chooser.setFiles(files);
  };
  const photo=(buffer,name='field.jpeg')=>({name,mimeType:'image/jpeg',buffer});
  try {
    await page.addInitScript(()=>{
      const fetch=window.fetch, heads=[];
      window.fixtureHeadUrls=heads;
      window.fetch=(url,opts)=>{
        if(opts?.method==='HEAD') heads.push(url);
        return fetch(url,opts);
      };
    });
    await page.goto(rig.origin,{waitUntil:'domcontentloaded'});
    await dom(page,'#app[data-phase="live"]');
    await page.locator(`#channels [data-channel="${direct}"]`).click();
    await dom(page,'#channel-form:not([hidden]) .compose-box');
    assert.equal(await plus('channel').count(),1,'a photo picker sits next to the main composer');
    await plus('channel').click();
    assert.equal(await choice('channel','Photos').count(),1,'+ offers Photos');
    assert.equal(await choice('channel','Files').count(),1,'+ offers Files');
    const layout=await plus('channel').evaluate(plus=>[plus.getBoundingClientRect().right,plus.closest('form').querySelector('.compose-box').getBoundingClientRect().left]);
    assert.ok(layout[0]<=layout[1],'the plus is left of the message box');
    await shot('attach-choices');
    await choice('channel','Files').press('Escape');
    assert.equal(await plus('channel').getAttribute('aria-expanded'),'false');
    const jpeg=Buffer.from(await page.evaluate(async()=>{
      const canvas=document.createElement('canvas');canvas.width=2500;canvas.height=1500;
      const ctx=canvas.getContext('2d');ctx.fillStyle='#268fbc';ctx.fillRect(0,0,2500,1500);
      return [...new Uint8Array(await (await new Promise(done=>canvas.toBlob(done,'image/jpeg'))).arrayBuffer())];
    }));
    const input=withGps(jpeg);assert.ok(input.includes('Exif'));
    const unreadable={name:'camera.heic',mimeType:'image/heic',buffer:Buffer.from('not an image')};
    await choose('channel','Photos',[unreadable]);
    await dom(page,'#channel-form [role="status"]',"Can't shrink this photo. Send it from Files to send it as is.");
    assert.equal(await page.locator('#channel-form .picks > *').count(),0,'Photos never stages raw bytes');
    assert.ok(await page.locator('#channel-form button[type="submit"]').isDisabled());
    await choose('channel','Photos',[photo(input)]);
    await dom(page,'#channel-form .picks canvas[width]');
    const pdf=tinyPdf();
    await choose('channel','Files',[{name:'report.pdf',mimeType:'application/pdf',buffer:pdf},unreadable]);
    await dom(page,'#channel-form .picks .file-chip small',`PDF · ${pdf.length} B`);
    await shot('file-picks');
    const from=wire.frames.rows.length;
    const names=['field.jpg','report.pdf','camera.heic'];
    const uploads=names.map(name=>page.waitForResponse(res=>res.request().method()==='POST'&&new URL(res.url()).searchParams.get('name')===name)
      .then(res=>{assert.equal(res.status(),201);return res.json();}));
    const [{path,size},paper,raw]=await Promise.all([Promise.all(uploads),page.locator('#channel-form button[type="submit"]').click()]).then(([files])=>files);
    for(const [i,file] of [{path},paper,raw].entries()) {
      assert.ok(realpathSync(file.path).startsWith(rig.store+sep));
      assert.ok(file.path.endsWith('/'+names[i]));
    }
    const stored=readFileSync(path);assert.equal(stored.length,size);assert.ok(!stored.includes('Exif'),'GPS-bearing EXIF never reaches the store');
    assert.deepEqual(readFileSync(paper.path),pdf);
    assert.deepEqual(readFileSync(raw.path),unreadable.buffer);
    const event=await wire.frames.next(e=>e.kind==='channel.posted'&&e.body.attachments?.includes(path),from);
    const id=`m${event.seq}`;assert.equal(event.body.channel,direct);assert.equal(event.body.body,'');
    const paths=[path,paper.path,raw.path];
    assert.deepEqual(event.body.attachments,paths);
    const wake=await wire.frames.next(e=>e.kind==='msg.sent'&&e.body.attachments?.includes(path),from);
    assert.equal(wake.body.to,'demo/lead');
    const inbox=rig.cli('lead','inbox','--id',`m${wake.seq}`);
    for(const attachment of paths)assert.ok(inbox.includes(`attach: ${attachment}`));
    const {posts}=await wire.rpc('channel.history',{channel:direct});
    assert.deepEqual(posts.filter(p=>p.attachments.includes(path)).map(p=>[p.id,p.attachments]),[[id,paths]]);
    assert.equal(wire.frames.rows.slice(from).filter(e=>e.kind==='msg.sent'&&e.body.attachments?.includes(path)).length,1);
    const message=`#posts [data-post-id="${id}"]`;
    const link=page.locator(message).getByRole('link',{name:/report\.pdf/});
    await dom(page,`${message} .file-chip small`,`PDF · ${pdf.length} B`);
    await dom(page,`${message} .file-chip small`,`HEIC · ${unreadable.buffer.length} B`);
    await shot('file-chips');
    const countHeads=()=>page.evaluate(url=>window.fixtureHeadUrls.filter(value=>value===url).length,
      '/file?p='+encodeURIComponent(paper.path));
    assert.equal(await countHeads(),1);
    const previous=await page.locator(message).elementHandle();
    await wire.rpc('channel.post',{reply_to:id,body:'Fixture reply refreshes the attachment row',client_key:'rebuild-file-chip'});
    await dom(page,`${message} .reply-count`,'1 reply');
    assert.equal(await previous.evaluate(row=>row.isConnected),false,'reply count really rebuilt the row');
    await dom(page,`${message} .file-chip small`,`PDF · ${pdf.length} B`);
    assert.equal(await countHeads(),1,'rebuilding the chip reuses the original HEAD');
    const url=page.url();
    const [download]=await Promise.all([page.waitForEvent('download'),link.click()]);
    assert.equal(download.suggestedFilename(),'report.pdf');
    assert.deepEqual(readFileSync(await download.path()),pdf);
    assert.equal(page.url(),url);
    const thumb=`${message} img`;
    await dom(page,thumb);
    const image=page.locator(thumb);await image.scrollIntoViewIfNeeded();
    assert.equal(await image.getAttribute('loading'),'lazy');
    assert.deepEqual(await image.evaluate(async img=>{await img.decode();return [img.naturalWidth,img.naturalHeight];}),[1229,2048]);
    await shot('thumbnail');await image.click();
    await dom(page,'#photo-view[open] img');
    await page.getByRole('dialog').getByRole('button',{name:'Close'}).click();
    await dom(page,'#photo-view:not([open])');assert.equal(new URL(page.url()).origin,rig.origin);
    await page.locator(`#posts [data-post-id="${directRoot}"]`).getByRole('button',{name:/^\d+ replies/}).click();
    await dom(page,'#thread-form:not([hidden]) .attach-plus');
    await choose('thread','Files',[{name:'report.pdf',mimeType:'application/pdf',buffer:pdf}]);
    await page.locator('#thread-form .picks').getByRole('button',{name:'Remove report.pdf'}).click();
    await dom(page,'#thread-form .picks[hidden]');
    await choose('thread','Photos',[photo(jpeg)]);
    await dom(page,'#thread-form .picks canvas[width]');
    await choose('thread','Photos',Array.from({length:8},(_,i)=>photo(jpeg,`extra-${i}.jpeg`)));
    await dom(page,'#thread-form [role="status"]','8 files at most');
    assert.equal(await page.locator('#thread-form .picks button').count(),8);
    assert.equal(await page.getByRole('button',{name:'Remove extra-7.jpg'}).count(),0);
    console.log('real files: one mixed DM post/wake; PDF and explicit raw HEIC unchanged, PDF download stays in chat; rotated EXIF-free photo, both pickers and shared limit');
    await agentFiles(page,rig,direct,dom,shot);
  } finally {await context.close();}
}
