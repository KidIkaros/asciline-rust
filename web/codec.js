/**
 * codec.js — Adaptive frame decoder for ASCILINE.
 *
 * Mirrors codec.py. Runs in the browser (attaches window.AscilineCodec) and in
 * Node (module.exports) so the end-to-end test exercises the exact shipped path.
 *
 * Wire format per binary frame:
 *   [4B frame_index big-endian][1B tag][payload]
 *   tag 0 RAW   : payload is the framebuffer bytes
 *   tag 1 ZLIB  : payload is zlib(framebuffer bytes)        -> 'deflate'
 *   tag 2 DELTA : payload is zlib(indices[uint32 LE] ++ changed values)
 *   tag 3 RLE   : payload is zlib(runs: [uint16 count][cell bytes]...)
 *   tag 4 PROFILE: opt-in lossy DCT profile (pixel mode), see PROFILE.md
 *   tag 5 PROFILE_AQ: tag 4 + per-block adaptive quantization (luma AQ map)
 *
 * Decoding MUST stay in arrival order (deltas patch the previous frame), so
 * callers feed messages through a sequential queue (see makeDecoder).
 */
(function (root, factory) {
  const api = factory();
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
  else root.AscilineCodec = api;
})(typeof self !== 'undefined' ? self : this, function () {
  const TAG_RAW = 0, TAG_ZLIB = 1, TAG_DELTA = 2, TAG_RLE_FULL = 3, TAG_PROFILE = 4, TAG_PROFILE_AQ = 5, TAG_PROFILE_HPEL = 6, TAG_PROFILE_QPEL = 7;

  async function inflate(bytes) {
    // Direct DecompressionStream pump — avoids the Blob+Response wrapper
    // allocations that the old approach created on every single frame decode.
    const ds = new DecompressionStream('deflate');
    const writer = ds.writable.getWriter();
    const reader = ds.readable.getReader();

    writer.write(bytes);
    writer.close();

    const chunks = [];
    let totalLen = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      chunks.push(value);
      totalLen += value.length;
    }

    if (chunks.length === 1) return chunks[0];
    const out = new Uint8Array(totalLen);
    let off = 0;
    for (const c of chunks) { out.set(c, off); off += c.length; }
    return out;
  }


  // ===== Opt-in lossy DCT profile (tag 4, pixel mode). Deterministic constants,
  // bit-exact with codec.py: integer IDCT and integer YUV420 -> BGR.
  // The hot path is allocation-free: per-block scratch is hoisted and reused, and
  // DC-only blocks skip the IDCT entirely (mathematically identical result). =====
  const _P_MI = Int32Array.from([23,23,23,23,23,23,23,23,31,27,18,6,-6,-18,-27,-31,30,12,-12,-30,-30,-12,12,30,
    27,-6,-31,-18,18,31,6,-27,23,-23,-23,23,23,-23,-23,23,18,-31,6,27,-27,-6,31,-18,
    12,-30,30,-12,-12,30,-30,12,6,-18,27,-31,31,-27,18,-6]);
  const _P_ZZ = Int32Array.from([0,1,8,16,9,2,3,10,17,24,32,25,18,11,4,5,12,19,26,33,40,48,41,34,27,20,13,6,7,14,
    21,28,35,42,49,56,57,50,43,36,29,22,15,23,30,37,44,51,58,59,52,45,38,31,39,46,53,60,61,54,47,55,62,63]);
  const _P_QLB=[16,11,10,16,24,40,51,61,12,12,14,19,26,58,60,55,14,13,16,24,40,57,69,56,14,17,22,29,51,87,80,62,
    18,22,37,56,68,109,103,77,24,35,55,64,81,104,113,92,49,64,78,87,103,121,120,101,72,92,95,98,112,100,103,99];
  const _P_QCB=[17,18,24,47,99,99,99,99,18,21,26,66,99,99,99,99,24,26,56,99,99,99,99,99,47,66,99,99,99,99,99,99,
    99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99,99];
  function _pqtables(QF){const S=QF<50?5000/QF:200-2*QF;const f=b=>{const o=new Int32Array(64);for(let i=0;i<64;i++){let v=Math.floor((b[i]*S+50)/100);o[i]=v<1?1:(v>255?255:v);}return o;};return [f(_P_QLB),f(_P_QCB)];}
  // Reused scratch. Decoding is strictly sequential, so sharing these is safe and
  // keeps the block loop free of allocations (GC pressure at high column counts).
  const _pT = new Float64Array(64);
  const _pO = new Int32Array(64);
  const _pZ = new Int32Array(64);
  const _pC = new Int32Array(64);
  const _pQ = new Int32Array(64); // per-block scaled quant table (tag 5 AQ)
  // Tag-5 AQ quant-step multipliers over 4, indexed by map value (index 0 =
  // coarsest for flat regions, last = finest for detail). Mirrors profile.rs.
  const _pAQ_2 = [4, 2];
  const _pAQ_4 = [6, 4, 3, 2];
  function _pScaleQm(qm, num) {
    for (let i = 0; i < 64; i++) {
      const v = Math.floor((qm[i] * num + 2) / 4);
      _pQ[i] = v < 1 ? 1 : v;
    }
    return _pQ;
  }
  function _pidct(C){
    for(let u=0;u<8;u++)for(let x=0;x<8;x++){let s=0;for(let v=0;v<8;v++)s+=C[u*8+v]*_P_MI[v*8+x];_pT[u*8+x]=s;}
    for(let y=0;y<8;y++)for(let x=0;x<8;x++){let s=0;for(let u=0;u<8;u++)s+=_P_MI[u*8+y]*_pT[u*8+x];_pO[y*8+x]=Math.floor((s+2048)/4096);}
    return _pO;
  }
  // Half-pel bilinear sample (tag 6): displacement (hdx,hdy) in half-pel
  // units; integer part hdx>>1 (floor for negatives), fractional bit hdx&1.
  // The four integer neighbors are edge-clamped; identical integer math to
  // the Rust encoder/decoder (hpel_sample in src/profile.rs).
  // H.264 6-tap half-pel filter: clip((A-5B+20C+20D-5E+F+16)>>5, 0, 255),
  // six input taps edge-clamped to the plane. Identical integer math to the
  // Rust encoder/decoder (h264_6tap in src/profile.rs). The accumulation
  // stays within 32 bits for byte inputs, so JS bitwise ops are exact.
  function _p6tap(buf,W,H,x,y){
    if(x<0)x=0; else if(x>=W)x=W-1;
    if(y<0)y=0; else if(y>=H)y=H-1;
    const row=y*W;
    const tap=(d)=>{let xc=x+d; if(xc<0)xc=0; else if(xc>=W)xc=W-1; return buf[row+xc];};
    const a=tap(-2),b=tap(-1),c=tap(0),d=tap(1),e=tap(2),f=tap(3);
    let v=(a-5*b+20*c+20*d-5*e+f+16)>>5;
    return v<0?0:(v>255?255:v);
  }
  // Quarter-pel sample (tag 7): displacement (qdx,qdy) in quarter-pel units;
  // integer part qdx>>2 (floor for negatives), fractional bits qdx&3. Integer
  // positions read the plane; half-pel (fx=2) use the 6-tap filter; quarter
  // positions bilinearly average the 6-tap half-pel with the ADJACENT integer
  // pixel ((A+B+1)>>1): pixel x for fx=1, pixel x+1 for fx=3 (and the same
  // vertically for fy). The half-pel step is H.264's separable filter
  // (horizontal 6-tap, then vertical 6-tap over the horizontal results).
  // Identical integer math to qpel_sample in src/profile.rs.
  function _pQpelSample(buf,W,H,px,py,qdx,qdy){
    let ix=px+(qdx>>2), iy=py+(qdy>>2);
    if(ix<0)ix=0; else if(ix>=W)ix=W-1;
    if(iy<0)iy=0; else if(iy>=H)iy=H-1;
    const fx=qdx&3, fy=qdy&3;
    // horizontal interpolation of row yr at fractional x
    const hx=(x,yr,f)=>{ if(f===0)return buf[yr*W+x]; const h=_p6tap(buf,W,H,x,yr); if(f===2)return h; const x1=x+1>=W?W-1:x+1; const v=(buf[yr*W+(f===1?x:x1)]+h+1)>>1; return v; };
    // half-pel interpolation of the whole column at fractional x
    const colHalf=(y)=>{ let y0=y-2,y1=y-1,y3=y+1,y4=y+2,y5=y+3;
      if(y0<0)y0=0; else if(y0>=H)y0=H-1; if(y1<0)y1=0; else if(y1>=H)y1=H-1;
      if(y3<0)y3=0; else if(y3>=H)y3=H-1; if(y4<0)y4=0; else if(y4>=H)y4=H-1;
      if(y5<0)y5=0; else if(y5>=H)y5=H-1;
      const c0=hx(ix,y0,fx),c1=hx(ix,y1,fx),c2=hx(ix,y,fx),c3=hx(ix,y3,fx),c4=hx(ix,y4,fx),c5=hx(ix,y5,fx);
      let v=(c0-5*c1+20*c2+20*c3-5*c4+c5+16)>>5;
      return v<0?0:(v>255?255:v);
    };
    let v;
    if(fy===0)v=hx(ix,iy,fx);
    else if(fy===2)v=colHalf(iy);
    else{ const y1=iy+1>=H?H-1:iy+1; v=(hx(ix,fy===1?iy:y1,fx)+colHalf(iy)+1)>>1; }
    return v<0?0:(v>255?255:v);
  }
  function _pHpelSample(buf,W,H,px,py,hdx,hdy){
    let ix=px+(hdx>>1), iy=py+(hdy>>1);
    if(ix<0)ix=0; else if(ix>=W)ix=W-1;
    if(iy<0)iy=0; else if(iy>=H)iy=H-1;
    const fx=hdx&1, fy=hdy&1;
    if(fx===0&&fy===0)return buf[iy*W+ix];
    let ix1=ix+1; if(ix1>=W)ix1=W-1;
    let iy1=iy+1; if(iy1>=H)iy1=H-1;
    const a=buf[iy*W+ix], b=buf[iy*W+ix1], c=buf[iy1*W+ix], d=buf[iy1*W+ix1];
    let v;
    if(fx===1&&fy===0)v=(a+b+1)>>1;
    else if(fx===0&&fy===1)v=(a+c+1)>>1;
    else v=(a+b+c+d+2)>>2;
    return v<0?0:(v>255?255:v);
  }
  function _pDecodePlane(data,off,P,NP,ft,useMv,mc,qm,aqLevels){
    const W=P.w,H=P.h,nbx=W>>3,nby=H>>3,nb=nbx*nby;
    let skipOut=null;
    // Tag-5 AQ map (luma only): log2(aqLevels) bits per block, MSB-first,
    // preceding the skip mask. Same packing math as the Rust encoder/decoder.
    const aqBits = aqLevels===2?1:(aqLevels===4?2:0);
    let aqNums = null;
    if(aqBits>0){
      const nbytes=(nb*aqBits+7)>>3;
      const map=data.subarray(off,off+nbytes); off+=nbytes;
      const mask=(1<<aqBits)-1;
      aqNums = new Int32Array(nb);
      const nums = aqLevels===2 ? _pAQ_2 : _pAQ_4;
      for(let bi=0;bi<nb;bi++){
        const bit=bi*aqBits, byte=bit>>3, shift=8-aqBits-(bit&7);
        aqNums[bi]=nums[(map[byte]>>shift)&mask];
      }
    }
    let skip=null; if(ft===1){const mb=(nb+7)>>3;skip=data.subarray(off,off+mb);skipOut=skip.slice();off+=mb;}
    let bi=0,dcPred=0;
    for(let by=0;by<nby;by++)for(let bx=0;bx<nbx;bx++){
      if(ft===1 && (skip[bi>>3]&(128>>(bi&7)))){bi++;continue;}
      const num = aqNums ? aqNums[bi] : 4;
      const qmb = num===4 ? qm : _pScaleQm(qm,num);
      let dx=0,dy=0;
      if(ft===1&&useMv){dx=(data[off]<<24>>24);dy=(data[off+1]<<24>>24);off+=2;}
      const nP=data[off++];
      _pZ.fill(0);
      let pos=0,lastNz=-1;
      for(let k=0;k<nP;k++){const run=data[off++];let v=data[off]|(data[off+1]<<8);off+=2;if(v&0x8000)v-=0x10000;pos+=run;_pZ[pos]=v;lastNz=pos;pos++;}
      _pZ[0]+=dcPred; dcPred=_pZ[0];
      // DC-only block: the first MI row is constant (23), so the IDCT collapses to a
      // flat value. Same integers, same rounding -> identical to the full transform.
      let res=null,flat=0;
      if(lastNz<=0){ flat=Math.floor((529*(_pZ[0]*qmb[0])+2048)/4096); }
      else { for(let k=0;k<64;k++){const id=_P_ZZ[k]; _pC[id]=_pZ[k]*qmb[id];} res=_pidct(_pC); }
      for(let y=0;y<8;y++){
        const row=(by*8+y)*W;
        for(let x=0;x<8;x++){
          let pred;
          if(ft===0)pred=128;
          else if(useMv&&mc===2)pred=_pQpelSample(P.buf,W,H,bx*8+x,by*8+y,dx,dy);
          else if(useMv&&mc===1)pred=_pHpelSample(P.buf,W,H,bx*8+x,by*8+y,dx,dy);
          else if(useMv){let sx=bx*8+x+dx,sy=by*8+y+dy;sx=sx<0?0:(sx>=W?W-1:sx);sy=sy<0?0:(sy>=H?H-1:sy);pred=P.buf[sy*W+sx];}
          else pred=P.buf[(by*8+y)*W+bx*8+x];
          const val=pred+(res===null?flat:res[y*8+x]);
          NP.buf[row+bx*8+x]=val<0?0:(val>255?255:val);
        }
      }
      bi++;
    }
    return {off, skip:skipOut};
  }
  function _pYuvToBgr(Y,Cb,Cr,W,H){const out=new Uint8Array(W*H*3);const cW=W>>1;
    for(let y=0;y<H;y++){const cy=y>>1;for(let x=0;x<W;x++){const cx=x>>1;const yy=Y[y*W+x];const cb=Cb[cy*cW+cx]-128;const cr=Cr[cy*cW+cx]-128;
      let R=yy+((359*cr+128)>>8),G=yy-((88*cb+183*cr+128)>>8),B=yy+((454*cb+128)>>8);const o=(y*W+x)*3;
      out[o]=B<0?0:(B>255?255:B);out[o+1]=G<0?0:(G>255?255:G);out[o+2]=R<0?0:(R>255?255:R);}}
    return out;}
  function makeProfileDecoder(){
    let W=0,H=0,cW=0,cH=0,planes=null,spare=null,QL=null,QC=null,aqL=0,mc=0; // mc: 0=integer (tag 4), 1=half-pel (tag 6), 2=quarter-pel (tag 7)
    const alloc=()=>[{w:W,h:H,buf:new Uint8Array(W*H)},{w:cW,h:cH,buf:new Uint8Array(cW*cH)},{w:cW,h:cH,buf:new Uint8Array(cW*cH)}];
    async function decode(message){
      const b=message instanceof Uint8Array?message:new Uint8Array(message);
      const dv=new DataView(b.buffer,b.byteOffset,b.byteLength);
      const idx=dv.getUint32(0,false); const tag=b[4]; const payload=await inflate(b.subarray(5)); const ft=payload[0];
      let off=1;
      if(ft===0){ // keyframe self-describes: [QF][cols u16][rows u16][aq_levels (tags 5/6)]
        const QF=payload[1]; const cols=(payload[2]<<8)|payload[3]; const rows=(payload[4]<<8)|payload[5];
        if(tag===TAG_PROFILE_AQ){ aqL=payload[6]; off=7; if(aqL!==2&&aqL!==4) throw new Error('profile AQ levels '+aqL+' out of bounds (2 or 4)'); }
        else if(tag===TAG_PROFILE_HPEL||tag===TAG_PROFILE_QPEL){ aqL=payload[6]; off=7; if(aqL!==0&&aqL!==2&&aqL!==4) throw new Error('profile AQ levels '+aqL+' out of bounds (0, 2 or 4)'); } // tags 6/7 always carry the byte (0 = AQ off)
        else { aqL=0; off=6; }
        mc = (tag===TAG_PROFILE_QPEL?2:(tag===TAG_PROFILE_HPEL?1:0)); // sub-pel motion (luma only)
        const q=_pqtables(QF); QL=q[0]; QC=q[1];
        if(planes===null||W!==cols||H!==rows){W=cols;H=rows;cW=W>>1;cH=H>>1;planes=alloc();spare=alloc();}
      }
      // ping-pong the plane buffers instead of allocating a new set every frame
      const out=spare;
      for(let i=0;i<3;i++) out[i].buf.set(planes[i].buf);
      for(let pi=0;pi<3;pi++){
        const r=_pDecodePlane(payload,off,planes[pi],out[pi],ft,pi===0,pi===0?mc:0,pi===0?QL:QC,pi===0?aqL:0);
        off=r.off;
      }
      spare=planes; planes=out;
      return {frameIndex:idx, frame:_pYuvToBgr(planes[0].buf,planes[1].buf,planes[2].buf,W,H)};
    }
    return {decode, reset(){planes=null;spare=null;QL=QC=null;aqL=0;mc=0;}};
  }

  /**
   * Create a stateful decoder. `cellBytes` = channels per cell (4 ASCII color,
   * 3 pixel). Returns { decode(message) -> {frameIndex, frame}, reset() }.
   * `frame` is a Uint8Array of the full framebuffer for that frame.
   */
  function makeDecoder(cellBytes) {
    let prev = null; // Uint8Array of last full frame
    let profileDec = null;

    async function decode(message) {
      const bytes = new Uint8Array(message);
      const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      const frameIndex = view.getUint32(0, false); // big-endian
      const tag = bytes[4];
      if (tag === TAG_PROFILE || tag === TAG_PROFILE_AQ || tag === TAG_PROFILE_HPEL || tag === TAG_PROFILE_QPEL) {
        if (!profileDec) profileDec = makeProfileDecoder();
        return await profileDec.decode(bytes);
      }
      const payload = bytes.subarray(5);

      let frame;
      if (tag === TAG_RAW) {
        frame = payload.slice(); // own copy; becomes next prev
      } else if (tag === TAG_ZLIB) {
        frame = await inflate(payload);
      } else if (tag === TAG_DELTA) {
        const body = await inflate(payload);
        const k = body.length / (4 + cellBytes);
        const idx = new DataView(body.buffer, body.byteOffset, body.byteLength);
        frame = prev.slice(); // patch onto a copy of previous frame
        const valuesOffset = k * 4;
        for (let j = 0; j < k; j++) {
          const cell = idx.getUint32(j * 4, true); // little-endian indices
          const dst = cell * cellBytes;
          const src = valuesOffset + j * cellBytes;
          for (let c = 0; c < cellBytes; c++) frame[dst + c] = body[src + c];
        }
      } else if (tag === TAG_RLE_FULL) {
        const body = await inflate(payload);
        const bodyView = new DataView(body.buffer, body.byteOffset, body.byteLength);
        let totalCells = 0;
        let offset = 0;
        while (offset < body.length) {
          totalCells += bodyView.getUint16(offset, true);
          offset += 2 + cellBytes;
        }
        frame = new Uint8Array(totalCells * cellBytes);
        offset = 0;
        let dst = 0;
        while (offset < body.length) {
          const count = bodyView.getUint16(offset, true);
          const valOffset = offset + 2;
          if (cellBytes === 4) {
            const v0 = body[valOffset], v1 = body[valOffset+1], v2 = body[valOffset+2], v3 = body[valOffset+3];
            for (let i = 0; i < count; i++) {
              frame[dst++] = v0; frame[dst++] = v1; frame[dst++] = v2; frame[dst++] = v3;
            }
          } else if (cellBytes === 3) {
            const v0 = body[valOffset], v1 = body[valOffset+1], v2 = body[valOffset+2];
            for (let i = 0; i < count; i++) {
              frame[dst++] = v0; frame[dst++] = v1; frame[dst++] = v2;
            }
          } else {
            for (let i = 0; i < count; i++) {
              for (let c = 0; c < cellBytes; c++) frame[dst++] = body[valOffset + c];
            }
          }
          offset += 2 + cellBytes;
        }
      } else {
        if (prev) return { frameIndex, frame: prev }; // graceful: repeat last frame on an unknown tag
        throw new Error('Unknown ASCILINE codec tag: ' + tag);
      }
      prev = frame;
      return { frameIndex, frame };
    }

    return { decode, reset() { prev = null; profileDec = null; } };
  }

  return { makeDecoder, makeProfileDecoder, inflate, TAG_RAW, TAG_ZLIB, TAG_DELTA, TAG_RLE_FULL, TAG_PROFILE, TAG_PROFILE_AQ, TAG_PROFILE_HPEL, TAG_PROFILE_QPEL };
});
