// WebRTC transport for one viewer: the scrcpy H.264 stream is packetized straight into RTP (no
// re-encode) on a send-only video track, and input arrives on a DataChannel. The existing viewer
// WebSocket carries the signaling (offer/answer/candidates).
import { EventEmitter } from 'node:events';
import { randomInt } from 'node:crypto';

let ndc = null;
try {
  ndc = (await import('node-datachannel')).default;
} catch (err) {
  console.warn(`WebRTC disabled (node-datachannel failed to load: ${err.message})`);
}

export const rtcAvailable = () => ndc !== null;

const PAYLOAD_TYPE = 96;
const CLOCK_RATE = 90000;
const MAX_FRAGMENT_SIZE = 1200; // stays under typical path MTU (incl. TURN/VPN overhead)
const NACK_HISTORY = 1024; // RTP packets kept for retransmission (~a keyframe and change)

/** Pulls PLI / FIR (keyframe requests) out of an RTCP compound packet. */
function hasKeyframeRequest(buf) {
  for (let off = 0; off + 4 <= buf.length; ) {
    const fmt = buf[off] & 0x1f;
    const pt = buf[off + 1];
    if ((buf[off] & 0xc0) !== 0x80) return false; // not RTCP v2
    if (pt === 206 && (fmt === 1 || fmt === 4)) return true;
    off += (buf.readUInt16BE(off + 2) + 1) * 4;
  }
  return false;
}

/**
 * Events: 'video-open', 'message' (Buffer|string from the DataChannel), 'keyframe-request',
 * 'connected' (selected candidate pair info), 'failed'.
 */
export class RtcPeer extends EventEmitter {
  /**
   * @param {object} o
   * @param {Array} o.iceServers node-datachannel format
   * @param {(msg: object) => void} o.signal sends a signaling message to the browser
   */
  constructor({ iceServers, signal }) {
    super();
    this.signal = signal;
    this.closed = false;
    this.lastPts = null;
    this.rtpTs = randomInt(0, 2 ** 31);

    const pc = new ndc.PeerConnection('ldplayer-agent', { iceServers, disableAutoNegotiation: true, maxMessageSize: 262144 });
    this.pc = pc;
    pc.onLocalDescription((sdp, type) => this.signal({ type: 'rtc-offer', sdp, sdpType: type }));
    pc.onLocalCandidate((candidate, mid) => this.signal({ type: 'rtc-candidate', candidate: candidate.replace(/^a=/, ''), mid }));
    pc.onStateChange((state) => {
      if (state === 'connected') {
        const pair = pc.getSelectedCandidatePair();
        this.emit('connected', pair ? `${pair.local.type}→${pair.remote.type} ${pair.remote.transportType}` : '');
      } else if (state === 'failed' || state === 'closed') {
        if (!this.closed) this.emit('failed', state);
      }
    });

    const ssrc = randomInt(1, 2 ** 31);
    const video = new ndc.Video('video', 'SendOnly');
    video.addH264Codec(PAYLOAD_TYPE);
    video.addSSRC(ssrc, 'ldplayer', 'ldplayer-stream', 'ldplayer-video');
    this.track = pc.addTrack(video);
    this.rtpConfig = new ndc.RtpPacketizationConfig(ssrc, 'ldplayer', PAYLOAD_TYPE, CLOCK_RATE);
    const packetizer = new ndc.H264RtpPacketizer('StartSequence', this.rtpConfig, MAX_FRAGMENT_SIZE);
    packetizer.addToChain(new ndc.RtcpSrReporter(this.rtpConfig));
    packetizer.addToChain(new ndc.RtcpNackResponder(NACK_HISTORY));
    this.track.setMediaHandler(packetizer);
    this.track.onOpen(() => this.emit('video-open'));
    this.track.onMessage((buf) => {
      if (hasKeyframeRequest(buf)) this.emit('keyframe-request');
    });

    this.dc = pc.createDataChannel('control');
    this.dc.onMessage((msg) => this.emit('message', msg instanceof ArrayBuffer ? Buffer.from(msg) : msg));

    pc.setLocalDescription();
  }

  onSignal(msg) {
    if (this.closed) return;
    try {
      if (msg.type === 'rtc-answer' && typeof msg.sdp === 'string') {
        this.pc.setRemoteDescription(msg.sdp, 'answer');
      } else if (msg.type === 'rtc-candidate' && typeof msg.candidate === 'string' && msg.candidate) {
        this.pc.addRemoteCandidate(msg.candidate, String(msg.mid ?? 'video'));
      }
    } catch (err) {
      console.warn(`rtc signaling error: ${err.message}`);
    }
  }

  isVideoOpen() {
    return !this.closed && this.track.isOpen();
  }

  /** Sends one access unit (Annex B). `ptsUs` is scrcpy's presentation time in microseconds. */
  sendFrame(annexB, ptsUs) {
    // scrcpy restarts PTS when the capture resets; RTP timestamps must keep moving forward or the
    // browser's jitter buffer treats new frames as stale.
    if (this.lastPts !== null) {
      const delta = ptsUs - this.lastPts;
      this.rtpTs += delta > 0 && delta < 1e6 ? Math.round(delta * 0.09) : 1500;
    }
    this.lastPts = ptsUs;
    this.rtpConfig.timestamp = this.rtpTs >>> 0;
    try {
      this.track.sendMessageBinary(annexB);
    } catch (err) {
      console.warn(`rtc send error: ${err.message}`);
    }
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    try {
      this.dc.close();
      this.track.close();
      this.pc.close();
    } catch {
      // already torn down
    }
  }
}
