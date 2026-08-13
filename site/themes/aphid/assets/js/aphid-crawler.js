/*
 * aphid-crawler — a mascot aphid that follows the cursor, keeping its distance.
 *
 * Movement is modelled after Rain World's creature physics: the body is a chain
 * of loose circular chunks held together by distance constraints, dragged around
 * by a single steered "leader" chunk (the head). The legs are not animated on a
 * clock — each foot is planted at an absolute page coordinate and simply stays
 * there while the body slides out from under it. Once a foot is stretched too
 * far from where it ought to be, it picks itself up and swings to a new hold
 * ahead of the body. The walk cycle is a side effect of that rule.
 *
 * Rendering is pixel art: the simulated skeleton is drawn at full precision
 * into a small offscreen buffer, then blitted back up with nearest-neighbor
 * scaling, so a fully continuous, smoothly rotating body still reads as
 * chunky and hand-placed like the mascot art it's based on. Press 'd' (or
 * load with ?aphid=debug) for a wireframe overlay of the underlying physics.
 */
(function () {
  'use strict';

  var CONFIG = {
    // --- simulation ---
    tickRate: 60, // Hz; velocities below are in px/tick, Rain World style
    maxSubSteps: 3,
    constraintIterations: 4,

    // --- body chunks: head, torso, abdomen ---
    chunks: [
      { name: 'head', rad: 9, mass: 0.6 },
      { name: 'torso', rad: 18, mass: 1.0 },
      { name: 'abdomen', rad: 7, mass: 0.5 }
    ],
    restHeadTorso: 22,
    restTorsoAbdomen: 20,
    antiFoldSlack: 0.9, // head<->abdomen min distance, as a fraction of the sum above
    drag: 0.86,

    // --- cursor following ---
    standoff: 120, // px it tries to keep between head and cursor
    band: 30, // dead zone half-width around the standoff distance
    // Terminal speed is gain / (1 - drag), so 0.42 / 0.14 = 3.0 px/tick, which
    // is where maxSpeed below is set. Keep the three in step when tuning.
    seekGain: 0.42,
    fleeGain: 0.6, // stronger than seek per unit of error: skittish
    seekRamp: 100, // px of error over which the seek force reaches full strength
    headLead: 1.5, // extra acceleration on the head so the body trails behind
    // --- facing ---
    // How fast the travel direction is allowed to rotate, per tick. Low values
    // make wide banking turns; high values let it pivot on the spot.
    headingTurnRate: 0.2,
    headingMinSpeed: 0.25, // per-chunk speed below which heading is held still
    // Spring pulling the head ahead of the torso and the abdomen behind it.
    // This trades against gait coherence — a body that turns briskly swings the
    // leg rest positions around and breaks the tripod — so it is set to the
    // strongest value that still keeps the tripod well above chance.
    alignGain: 0.08,
    facingSmooth: 0.35, // how quickly the drawn facing catches up to the chain
    // Speed is bounded by the gait, not by taste: the distance covered during
    // one swing (maxSpeed * tickRate * stepDuration) has to stay well under the
    // stride, or every leg spends more time in the air than on the ground and
    // the walk falls apart. Raising this means shortening stepDuration too.
    maxSpeed: 3.0, // px/tick
    cursorSmooth: 0.25,
    // --- resting ---
    // Inside the standoff dead zone the aphid brakes and then latches to a
    // full stop. Without the latch it drifts on residual velocity and constraint
    // feedback forever, which shows up as the legs twitching in place: the body
    // creeps a fraction of a pixel, a foot passes its step threshold, it steps,
    // and that nudge starts the cycle again.
    restBraking: 0.72, // extra velocity damping while in the dead zone
    restSpeed: 0.06, // px/tick below which the body counts as stopped
    restAfterTicks: 8, // consecutive slow ticks before the full stop latches
    edgeMargin: 24,

    // --- legs: 3 pairs ---
    // Six legs walk the classic insect tripod: front-left, middle-right and
    // rear-left swing together while the other three hold the ground. That
    // falls out of the phase grouping below rather than being scripted.
    legPairs: 3,
    // Position along the head->torso->abdomen spine, where 0 is the head, 0.5
    // the torso and 1 the abdomen. All six legs hang off the torso like an
    // insect's thorax — spread just fore and aft of it, never on the head.
    attachT: [0.32, 0.5, 0.68],
    // Angle off straight-out-to-the-side, mirrored per side. Negative rakes a
    // leg forward, positive trails it back. Kept shallow so the legs open out
    // sideways instead of reaching past the front of the body.
    splayDeg: [-14, 20, 54],
    hipOffset: -3, // tucked slightly inside the body outline (see spineRadius)
    reach: 24, // distance from hip to the resting foot position
    stepThreshold: 9, // stretch from the rest position that asks for a step
    // Past this a leg steps even if the gait rules say no. The gap between this
    // and stepThreshold is the patience a leg has while waiting its turn: too
    // narrow and every leg goes urgent, bypasses the ordering rule and the
    // tripod degenerates towards random. Measured 66% -> 76% tripod coherence
    // widening it from 15 to 23.
    forceStepAt: 23,
    stepDuration: 0.055, // seconds; a swing must be far shorter than a stride
    // A foot must land *ahead* of its rest position, not on it, or it arrives
    // already nearly out of tolerance and steps again a couple of ticks later.
    // Landing `stepLead` ahead lets it drift through rest and out the far side,
    // which is what gives the leg a full stride and a sane duty cycle.
    stepLead: 11,
    stepAnticipation: 3.2, // extra lead proportional to speed
    leadFullSpeed: 1.0, // px/tick at which the step lead reaches full size
    stepJitter: 3,
    stepArc: 7, // perpendicular bow of the swing path
    maxSteppingLegs: 3, // one full tripod may be in the air, never more
    // Every foot stretches at the same rate, so left to itself the whole set
    // reaches the step threshold on the same tick and the gait lock spends its
    // time blocking. Each leg therefore lands a fixed distance fore or aft of
    // its rest position, in an alternating tripod pattern. That bias is
    // re-applied on every landing, so the legs stay permanently out of phase
    // instead of re-syncing each stride.
    phaseBias: 6,
    // Span is ~2x `reach`. That is generous, but the slack is what lets a
    // planted foot fall behind the body without hitting its tether, and
    // measurement showed anything under ~1.85x makes the tether fire constantly
    // and shred the gait. The knee excursion it costs is fine once the bend
    // direction is stable (see solveIK). Scale these with `reach`, not alone.
    femur: 25,
    tibia: 23,
    kneeBend: -1, // which way the knee folds; flip to mirror the joint
    // Kept meaningfully below footTetherLimit: a foothold landed right at the
    // tether boundary has zero slack before the next tick's hip movement makes
    // it critical again, so the leg immediately re-steps and never rests —
    // seen as legs shaking instead of walking.
    footAnchorLimit: 0.8, // fraction of the leg span a new foothold may sit at
    // Tether: a planted foot this far from its own hip is at the edge of what
    // the bones can span, so the leg steps immediately no matter what the gait
    // rules say. This is keyed off the hip rather than the rest position
    // because it is the actual physical limit — it is what guarantees the drawn
    // leg never detaches from its foothold.
    footTetherLimit: 0.92,

    // --- antennae ---
    // Purely cosmetic — no constraint, no gait interaction. Drawn as a bowed
    // curve from the head with a slow idle sway so the head doesn't read as
    // static even at a full stop.
    antennaLength: 34,
    antennaSpread: 15,
    antennaSway: 5, // px of extra spread added by the idle sine
    antennaSwayPeriod: 1400, // ms per sway cycle

    // --- render ---
    debug: false,
    footDotRad: 2.5,
    // World px per art "pixel". The creature is rendered at full precision
    // into a low-res offscreen buffer and blitted back up with nearest-
    // neighbor scaling, which is what gives it the same chunky, hard-edged
    // look as the reference mascot art regardless of the smooth curves and
    // continuous rotation driving it underneath.
    pixelUnit: 2
  };

  // Bail out before touching anything if the environment says no.
  var reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)');
  var coarsePointer = window.matchMedia('(pointer: coarse)');
  var params = new URLSearchParams(window.location.search);
  var mode = params.get('aphid');
  if (mode === 'off') return;
  if (reduceMotion.matches || coarsePointer.matches) return;
  if (mode === 'debug') CONFIG.debug = true;

  // ---------------------------------------------------------------- vectors

  function v(x, y) { return { x: x, y: y }; }
  function set(a, x, y) { a.x = x; a.y = y; return a; }
  function copy(a) { return v(a.x, a.y); }
  function addTo(a, b, s) { a.x += b.x * s; a.y += b.y * s; return a; }
  function sub(a, b) { return v(a.x - b.x, a.y - b.y); }
  function scale(a, s) { return v(a.x * s, a.y * s); }
  function len(a) { return Math.sqrt(a.x * a.x + a.y * a.y); }
  function dist(a, b) { return Math.hypot(a.x - b.x, a.y - b.y); }
  function norm(a) {
    var l = len(a);
    return l > 1e-6 ? v(a.x / l, a.y / l) : v(0, 0);
  }
  function perp(a) { return v(-a.y, a.x); }
  function rot(a, ang) {
    var c = Math.cos(ang), s = Math.sin(ang);
    return v(a.x * c - a.y * s, a.x * s + a.y * c);
  }
  function lerp(a, b, t) { return a + (b - a) * t; }
  function lerpV(a, b, t) { return v(lerp(a.x, b.x, t), lerp(a.y, b.y, t)); }
  function easeInOutQuad(t) {
    return t < 0.5 ? 2 * t * t : 1 - Math.pow(-2 * t + 2, 2) / 2;
  }
  function rad(deg) { return (deg * Math.PI) / 180; }

  // ------------------------------------------------------------------ state

  var body = null;
  var legs = [];
  var cursor = v(0, 0); // smoothed
  var rawCursor = v(0, 0);
  var haveCursor = false;
  var bodyAngle = -Math.PI / 2;
  // Smoothed direction of travel. The body turns to face this and the head is
  // pulled around to lead, so the aphid always walks head-first instead of
  // sliding sideways or backing up when it changes direction.
  var heading = v(0, -1);
  var resting = false;
  var restTicks = 0;
  var viewW = 0, viewH = 0;

  function createBody(cx, cy) {
    var chunks = CONFIG.chunks.map(function (spec, i) {
      // Lay the chain out pointing straight up, head first.
      var y = cy + i * CONFIG.restHeadTorso;
      return {
        name: spec.name,
        pos: v(cx, y),
        vel: v(0, 0),
        rad: spec.rad,
        mass: spec.mass,
        invMass: 1 / spec.mass
      };
    });

    var minSpan = (CONFIG.restHeadTorso + CONFIG.restTorsoAbdomen) * CONFIG.antiFoldSlack;
    return {
      chunks: chunks,
      head: chunks[0],
      torso: chunks[1],
      abdomen: chunks[2],
      connections: [
        { a: 0, b: 1, rest: CONFIG.restHeadTorso, pushOnly: false },
        { a: 1, b: 2, rest: CONFIG.restTorsoAbdomen, pushOnly: false },
        // Anti-fold: the body may curve when turning, but never double back.
        { a: 0, b: 2, rest: minSpan, pushOnly: true }
      ]
    };
  }

  // -------------------------------------------------------------- physics

  function steerBody() {
    var head = body.head;
    var toCursor = sub(cursor, head.pos);
    var d = len(toCursor);
    var dir = norm(toCursor);
    var acc = v(0, 0);

    if (d > CONFIG.standoff + CONFIG.band) {
      var gain = Math.min(1, (d - CONFIG.standoff) / CONFIG.seekRamp);
      acc = scale(dir, CONFIG.seekGain * gain);
    } else if (d < CONFIG.standoff - CONFIG.band) {
      var flee = Math.min(1, (CONFIG.standoff - d) / CONFIG.seekRamp);
      acc = scale(dir, -CONFIG.fleeGain * flee);
    } else {
      // At its preferred distance: brake rather than steer, so it comes to an
      // actual stop instead of milling around inside the dead zone.
      for (var b = 0; b < body.chunks.length; b++) {
        var ch = body.chunks[b];
        ch.vel.x *= CONFIG.restBraking;
        ch.vel.y *= CONFIG.restBraking;
      }
      return false;
    }

    // Accelerate every chunk, not just the head. Steering the head alone means
    // it has to drag the rest of the body through the constraints, which bleeds
    // most of the force away and caps the real speed far below `maxSpeed`.
    // The head still gets the larger share, so the body trails and curves.
    for (var i = 0; i < body.chunks.length; i++) {
      var c = body.chunks[i];
      addTo(c.vel, acc, c === head ? CONFIG.headLead : 1);
      var speed = len(c.vel);
      if (speed > CONFIG.maxSpeed) {
        c.vel.x *= CONFIG.maxSpeed / speed;
        c.vel.y *= CONFIG.maxSpeed / speed;
      }
    }
    return true;
  }

  function solveConnections() {
    var chunks = body.chunks;
    for (var it = 0; it < CONFIG.constraintIterations; it++) {
      for (var i = 0; i < body.connections.length; i++) {
        var c = body.connections[i];
        var a = chunks[c.a], b = chunks[c.b];
        var delta = sub(b.pos, a.pos);
        var d = len(delta);
        if (d < 1e-6) {
          // Degenerate overlap: nudge apart along an arbitrary axis.
          delta = v(0.01, 0);
          d = 0.01;
        }
        if (c.pushOnly && d >= c.rest) continue;

        var error = d - c.rest;
        var n = scale(delta, 1 / d);
        var invSum = a.invMass + b.invMass;
        var aShare = (error * a.invMass) / invSum;
        var bShare = (error * b.invMass) / invSum;

        addTo(a.pos, n, aShare);
        addTo(b.pos, n, -bShare);
        // Feed the correction back into velocity so the chain has some whip.
        addTo(a.vel, n, aShare * 0.5);
        addTo(b.vel, n, -bShare * 0.5);
      }
    }
  }

  function clampToView() {
    var m = CONFIG.edgeMargin;
    for (var i = 0; i < body.chunks.length; i++) {
      var c = body.chunks[i];
      var minX = m + c.rad, maxX = viewW - m - c.rad;
      var minY = m + c.rad, maxY = viewH - m - c.rad;
      if (c.pos.x < minX) { c.pos.x = minX; if (c.vel.x < 0) c.vel.x = 0; }
      if (c.pos.x > maxX) { c.pos.x = maxX; if (c.vel.x > 0) c.vel.x = 0; }
      if (c.pos.y < minY) { c.pos.y = minY; if (c.vel.y < 0) c.vel.y = 0; }
      if (c.pos.y > maxY) { c.pos.y = maxY; if (c.vel.y > 0) c.vel.y = 0; }
    }
  }

  // Tracks the direction the body is actually travelling. Below the speed
  // floor there is no meaningful direction to read off a near-zero velocity, so
  // the last heading is kept rather than letting it spin on noise.
  function updateHeading() {
    var vel = v(0, 0);
    for (var i = 0; i < body.chunks.length; i++) addTo(vel, body.chunks[i].vel, 1);
    var speed = len(vel);
    if (speed < CONFIG.headingMinSpeed * body.chunks.length) return;

    var target = scale(vel, 1 / speed);
    heading = norm(lerpV(heading, target, CONFIG.headingTurnRate));
  }

  // Swings the head around to lead and lets the abdomen trail, so a change of
  // direction becomes a turn rather than the creature sliding off sideways.
  function alignToHeading() {
    var torso = body.torso;
    var head = body.head;
    var aheadX = torso.pos.x + heading.x * CONFIG.restHeadTorso;
    var aheadY = torso.pos.y + heading.y * CONFIG.restHeadTorso;

    // The tail extends the head->torso segment's own direction rather than
    // pointing along the same global `heading` as the head. That segment
    // already lags the head through a distance constraint, so continuing it
    // is what makes the spine bow through a turn — the curve travels down
    // the body instead of every chunk snapping to face the same way at once.
    var spineDir = norm(sub(torso.pos, head.pos));
    var behindX = torso.pos.x + spineDir.x * CONFIG.restTorsoAbdomen;
    var behindY = torso.pos.y + spineDir.y * CONFIG.restTorsoAbdomen;

    addTo(head.vel, v(aheadX - head.pos.x, aheadY - head.pos.y), CONFIG.alignGain);
    addTo(body.abdomen.vel,
      v(behindX - body.abdomen.pos.x, behindY - body.abdomen.pos.y), CONFIG.alignGain);
  }

  // Latches a complete stop once the aphid is at its preferred distance and has
  // slowed to a crawl. Braking alone is not enough: velocity decays
  // asymptotically and the constraint solver keeps feeding a little back in, so
  // without a hard latch there is always enough residual motion to keep
  // tripping the leg step threshold.
  function updateRestState() {
    var d = dist(body.head.pos, cursor);
    if (Math.abs(d - CONFIG.standoff) > CONFIG.band) {
      resting = false;
      restTicks = 0;
      return;
    }

    var fastest = 0;
    for (var i = 0; i < body.chunks.length; i++) {
      fastest = Math.max(fastest, len(body.chunks[i].vel));
    }
    // Never latch mid-stride, or a leg freezes with its foot in the air.
    if (fastest < CONFIG.restSpeed && steppingCount() === 0) restTicks++;
    else restTicks = 0;

    if (restTicks >= CONFIG.restAfterTicks) resting = true;
  }

  function stepPhysics(dt) {
    cursor = lerpV(cursor, rawCursor, CONFIG.cursorSmooth);
    updateRestState();

    if (resting) {
      // Fully asleep: hold every chunk exactly where it is. No integration, no
      // constraint relaxation, nothing that could nudge a foot.
      for (var r = 0; r < body.chunks.length; r++) {
        set(body.chunks[r].vel, 0, 0);
      }
      return;
    }

    // Skip heading/alignment while merely braking in the dead zone: with no
    // real seek or flee force behind it, `heading` has nothing to track but
    // constraint-solver whip noise, and the align spring chasing that noisy
    // heading is what was seen as the body spinning in place near the cursor.
    if (steerBody()) {
      updateHeading();
      alignToHeading();
    }

    for (var i = 0; i < body.chunks.length; i++) {
      var c = body.chunks[i];
      c.pos.x += c.vel.x;
      c.pos.y += c.vel.y;
      c.vel.x *= CONFIG.drag;
      c.vel.y *= CONFIG.drag;
    }

    solveConnections();
    clampToView();

    // Facing is read back off the chain rather than from `heading` directly, so
    // the body visibly lags and curves through a turn instead of snapping to
    // the new direction.
    var fwd = sub(body.head.pos, body.torso.pos);
    if (len(fwd) > 1e-3) {
      var target = Math.atan2(fwd.y, fwd.x);
      var delta = target - bodyAngle;
      while (delta > Math.PI) delta -= Math.PI * 2;
      while (delta < -Math.PI) delta += Math.PI * 2;
      bodyAngle += delta * CONFIG.facingSmooth;
    }
  }

  // ----------------------------------------------------------------- legs

  function createLegs() {
    var out = [];
    for (var pair = 0; pair < CONFIG.legPairs; pair++) {
      for (var s = 0; s < 2; s++) {
        var side = s === 0 ? -1 : 1;
        // Alternating tripod grouping: diagonally opposite legs share a phase.
        var phase = (pair + (side === 1 ? 1 : 0)) % 2 === 0 ? -1 : 1;
        out.push({
          pair: pair,
          side: side,
          phase: phase,
          foot: v(0, 0),
          stepFrom: v(0, 0),
          stepTo: v(0, 0),
          stepCtrl: v(0, 0),
          stepT: 0,
          stepping: false,
          hip: v(0, 0),
          rest: v(0, 0)
        });
      }
    }
    return out;
  }

  // The hip rides the head->torso->abdomen spine: t in [0,0.5] walks the first
  // segment, [0.5,1] the second.
  function spinePoint(t) {
    var h = body.head.pos, m = body.torso.pos, a = body.abdomen.pos;
    return t < 0.5 ? lerpV(h, m, t * 2) : lerpV(m, a, (t - 0.5) * 2);
  }

  // Body outline radius at the same spine point, so the hip can be anchored to
  // the surface of the body rather than to the (differently-scaled) spine.
  function spineRadius(t) {
    var h = body.head.rad, m = body.torso.rad, a = body.abdomen.rad;
    return t < 0.5 ? lerp(h, m, t * 2) : lerp(m, a, (t - 0.5) * 2);
  }

  function updateLegAnchors() {
    var fwd = v(Math.cos(bodyAngle), Math.sin(bodyAngle));
    var right = perp(fwd);
    for (var i = 0; i < legs.length; i++) {
      var leg = legs[i];
      var base = spinePoint(CONFIG.attachT[leg.pair]);
      // Root the leg near the body outline, not at a fixed distance from the
      // spine, so hips track the head/torso/abdomen taper instead of sitting
      // inside the torso once it grows past a fixed distance. `hipOffset` is
      // negative to tuck the socket slightly under the outline rather than
      // right on it, so the body reads as covering the leg root.
      var hipDist = spineRadius(CONFIG.attachT[leg.pair]) + CONFIG.hipOffset;
      set(leg.hip,
        base.x + right.x * leg.side * hipDist,
        base.y + right.y * leg.side * hipDist);

      // Straight out to the side is splay 0; negative angles rake the leg
      // forward, positive ones trail it back. Front pairs reaching forward and
      // rear pairs trailing is what reads as an insect rather than a spider.
      var legAngle = bodyAngle + leg.side * (Math.PI / 2 + rad(CONFIG.splayDeg[leg.pair]));
      var dir = v(Math.cos(legAngle), Math.sin(legAngle));
      set(leg.rest,
        leg.hip.x + dir.x * CONFIG.reach,
        leg.hip.y + dir.y * CONFIG.reach);
    }
  }

  // Hard anatomical rule: an insect never lifts both legs of a pair at once.
  function pairMateStepping(leg) {
    for (var i = 0; i < legs.length; i++) {
      var o = legs[i];
      if (o !== leg && o.stepping && o.pair === leg.pair) return true;
    }
    return false;
  }

  // Soft rule: keeps the travelling wave down each side. Yields to urgency.
  function neighbourBusy(leg) {
    for (var i = 0; i < legs.length; i++) {
      var o = legs[i];
      if (o === leg || !o.stepping) continue;
      if (o.side === leg.side && Math.abs(o.pair - leg.pair) === 1) return true;
    }
    return false;
  }

  function steppingCount() {
    var n = 0;
    for (var i = 0; i < legs.length; i++) if (legs[i].stepping) n++;
    return n;
  }

  // Keeps a foot inside the radius its two bones can actually reach.
  function tetherFoot(leg) {
    var limit = (CONFIG.femur + CONFIG.tibia) * CONFIG.footAnchorLimit;
    var delta = sub(leg.foot, leg.hip);
    var d = len(delta);
    if (d <= limit) return;
    set(leg.foot, leg.hip.x + (delta.x / d) * limit, leg.hip.y + (delta.y / d) * limit);
  }

  function beginStep(leg) {
    // A foot that triggered a critical step is, by definition, at or past the
    // limit of its bones, and a fast turn can swing the hip further still in
    // the tick before the swing starts. Pull it in at the moment it lifts off:
    // it is no longer planted, so moving it is legitimate, and this is what
    // makes "foot always within leg span" hold by construction.
    tetherFoot(leg);

    var vel = body.torso.vel;
    var speed = len(vel);
    // Lead along the direction of travel; when nearly stationary there is no
    // meaningful travel direction, so fall back to the body's facing.
    var travel = speed > 0.05 ? scale(vel, 1 / speed) : v(Math.cos(bodyAngle), Math.sin(bodyAngle));
    // The lead has to fade out with speed. At a standstill there is no "ahead"
    // to aim at, and a fixed lead would drop the foot further from its rest
    // position than the step threshold allows — so it would trigger another
    // step on landing, forever. That is what made the legs twitch on the spot
    // once the aphid reached its standoff distance.
    var leadScale = Math.min(1, speed / CONFIG.leadFullSpeed);
    var lead = (CONFIG.stepLead + speed * CONFIG.stepAnticipation) * leadScale;
    var bias = scale(v(Math.cos(bodyAngle), Math.sin(bodyAngle)), leg.phase * CONFIG.phaseBias);

    leg.stepFrom = copy(leg.foot);
    // Land ahead of the rest position, carrying the leg's phase bias so the
    // gait does not re-sync every stride.
    set(leg.stepTo,
      leg.rest.x + travel.x * lead + bias.x + (Math.random() - 0.5) * CONFIG.stepJitter,
      leg.rest.y + travel.y * lead + bias.y + (Math.random() - 0.5) * CONFIG.stepJitter);

    // Never plant a foothold the leg cannot actually reach.
    var limit = (CONFIG.femur + CONFIG.tibia) * CONFIG.footAnchorLimit;
    var fromHip = sub(leg.stepTo, leg.hip);
    var reachD = len(fromHip);
    if (reachD > limit) {
      set(leg.stepTo,
        leg.hip.x + (fromHip.x / reachD) * limit,
        leg.hip.y + (fromHip.y / reachD) * limit);
    }

    // Bow the swing path outward, away from the body.
    var mid = lerpV(leg.stepFrom, leg.stepTo, 0.5);
    var outward = norm(sub(mid, leg.hip));
    set(leg.stepCtrl,
      mid.x + outward.x * CONFIG.stepArc,
      mid.y + outward.y * CONFIG.stepArc);

    leg.stepT = 0;
    leg.stepping = true;
  }

  function updateLegs(dt) {
    updateLegAnchors();

    // 1. Advance the legs already in flight.
    for (var i = 0; i < legs.length; i++) {
      var leg = legs[i];
      if (!leg.stepping) continue;

      leg.stepT += dt / CONFIG.stepDuration;
      if (leg.stepT >= 1) {
        leg.stepT = 1;
        leg.stepping = false;
        leg.foot = copy(leg.stepTo);
      } else {
        var t = easeInOutQuad(leg.stepT);
        var u = 1 - t;
        // Quadratic Bezier: from -> ctrl -> to.
        set(leg.foot,
          u * u * leg.stepFrom.x + 2 * u * t * leg.stepCtrl.x + t * t * leg.stepTo.x,
          u * u * leg.stepFrom.y + 2 * u * t * leg.stepCtrl.y + t * t * leg.stepTo.y);
      }
      // The hip keeps moving under a swinging foot, and the swing arc bows
      // outward, so the mid-flight foot can leave the leg's range even though
      // both its endpoints were in range. Clamp it here so the invariant holds
      // for swinging legs too, not just planted ones.
      tetherFoot(leg);
    }

    // Asleep: in-flight steps above still finish, but nothing new may start.
    // That is what actually holds the legs still at the standoff distance.
    if (resting) return;

    // 2. Collect the planted legs that want to move. A planted `foot` is an
    // absolute page coordinate and is never written — the body slides out from
    // under it, and only this scheduler may pick it up.
    var tether = (CONFIG.femur + CONFIG.tibia) * CONFIG.footTetherLimit;
    var wanting = [];
    for (var j = 0; j < legs.length; j++) {
      var l = legs[j];
      if (l.stepping) continue;
      var stretch = dist(l.foot, l.rest);
      var critical = dist(l.foot, l.hip) > tether;
      if (stretch > CONFIG.stepThreshold || critical) {
        wanting.push({ leg: l, stretch: stretch, critical: critical });
      }
    }
    if (!wanting.length) return;

    // 3. Serve the most overstretched legs first. Ordering by urgency rather
    // than by array index is what stops the rear legs from being starved by the
    // front ones and dragged into a permanent overstretch.
    wanting.sort(function (a, b) {
      if (a.critical !== b.critical) return a.critical ? -1 : 1;
      return b.stretch - a.stretch;
    });

    for (var k = 0; k < wanting.length; k++) {
      var cand = wanting[k].leg;
      // A leg at the end of its tether steps no matter what: a leg visibly
      // detached from its own foot looks far worse than a broken gait rule.
      if (wanting[k].critical) { beginStep(cand); continue; }
      // Otherwise the soft rules apply, with `urgent` overriding the cap and
      // the travelling-wave rule but never the pair rule.
      var urgent = wanting[k].stretch > CONFIG.forceStepAt;
      if (pairMateStepping(cand)) continue;
      if (!urgent && (steppingCount() >= CONFIG.maxSteppingLegs || neighbourBusy(cand))) continue;
      beginStep(cand);
    }
  }

  function plantAllFeet() {
    updateLegAnchors();
    var fwd = v(Math.cos(bodyAngle), Math.sin(bodyAngle));
    for (var i = 0; i < legs.length; i++) {
      var leg = legs[i];
      // Start already staggered, so the gait is in phase from the first tick.
      var bias = leg.phase * CONFIG.phaseBias;
      leg.foot = v(leg.rest.x + fwd.x * bias, leg.rest.y + fwd.y * bias);
      leg.stepping = false;
      leg.stepT = 0;
    }
  }

  // ------------------------------------------------------------------- IK

  // Two-bone IK. The knee is found by rotating off the hip->foot direction by
  // the law-of-cosines interior angle, with the bend direction fixed by the
  // leg's own side.
  //
  // The obvious alternative — taking the circle-circle intersection on the side
  // nearest some outward reference vector — is unstable: the reference goes
  // perpendicular to the bend axis exactly when the leg points straight out to
  // the side, which is the rest pose for the middle pairs, so the knee flips
  // sides frame to frame and the legs cross each other. Rotating by a signed
  // angle can never flip.
  function solveIK(hip, foot, femur, tibia, bendSign) {
    var delta = sub(foot, hip);
    var d = len(delta);
    if (d < 1e-6) return copy(hip);
    var n = scale(delta, 1 / d);

    // Clamp into the range the two bones can actually resolve.
    var maxD = femur + tibia - 0.001;
    var minD = Math.abs(femur - tibia) + 0.001;
    if (d > maxD) d = maxD;
    if (d < minD) d = minD;

    var cosA = (femur * femur + d * d - tibia * tibia) / (2 * femur * d);
    if (cosA > 1) cosA = 1;
    if (cosA < -1) cosA = -1;
    var dir = rot(n, bendSign * Math.acos(cosA));
    return v(hip.x + dir.x * femur, hip.y + dir.y * femur);
  }

  function clampedFoot(leg) {
    var maxD = CONFIG.femur + CONFIG.tibia - 0.001;
    var delta = sub(leg.foot, leg.hip);
    var d = len(delta);
    if (d <= maxD) return leg.foot;
    var n = scale(delta, maxD / d);
    return v(leg.hip.x + n.x, leg.hip.y + n.y);
  }

  // --------------------------------------------------------------- render

  var COLORS = {
    ink: '#263524',
    moss: '#61814a',
    blossom: '#d98fac',
    lavender: '#9d8fc9'
  };

  // Sampled directly off site/themes/aphid/assets/images/aphid-mascot.png so
  // the crawler reads as the same creature, not just a same-colored one. Not
  // theme tokens: the mascot art doesn't move with --moss et al., so neither
  // does this.
  var ART = {
    body: '#7ba337',
    bodyShadow: '#5d7d25',
    bodyHighlight: '#94b860',
    leg: '#848825',
    legDark: '#616416',
    eye: '#100c03'
  };

  function readThemeColors() {
    var cs = getComputedStyle(document.documentElement);
    var map = { ink: '--ink', moss: '--moss', blossom: '--blossom', lavender: '--lavender' };
    Object.keys(map).forEach(function (k) {
      var val = cs.getPropertyValue(map[k]).trim();
      if (val) COLORS[k] = val;
    });
  }

  function circle(ctx, p, r) {
    ctx.beginPath();
    ctx.arc(p.x, p.y, r, 0, Math.PI * 2);
  }

  // Both antennae are purely cosmetic (see CONFIG) and share one shape: a
  // bowed curve off the head that tapers back in toward the tip, plus a slow
  // sideways sway so the head has some life in it even at a full stop.
  function antennaPoints(side, now) {
    var fwd = v(Math.cos(bodyAngle), Math.sin(bodyAngle));
    var right = perp(fwd);
    var head = body.head;
    var sway = Math.sin(now / CONFIG.antennaSwayPeriod + side * 1.3) * CONFIG.antennaSway;
    var spread = CONFIG.antennaSpread + sway;

    var base = v(
      head.pos.x + right.x * side * head.rad * 0.5 - fwd.x * head.rad * 0.2,
      head.pos.y + right.y * side * head.rad * 0.5 - fwd.y * head.rad * 0.2
    );
    var ctrl = v(
      base.x + fwd.x * CONFIG.antennaLength * 0.5 + right.x * side * spread,
      base.y + fwd.y * CONFIG.antennaLength * 0.5 + right.y * side * spread
    );
    var tip = v(
      base.x + fwd.x * CONFIG.antennaLength + right.x * side * spread * 0.4,
      base.y + fwd.y * CONFIG.antennaLength + right.y * side * spread * 0.4
    );
    return { base: base, ctrl: ctrl, tip: tip };
  }

  function drawAntennaeArt(c, now) {
    c.strokeStyle = ART.leg;
    c.lineWidth = 3;
    [-1, 1].forEach(function (side) {
      var a = antennaPoints(side, now);
      c.beginPath();
      c.moveTo(a.base.x, a.base.y);
      c.quadraticCurveTo(a.ctrl.x, a.ctrl.y, a.tip.x, a.tip.y);
      c.stroke();
      c.fillStyle = ART.legDark;
      circle(c, a.tip, 2);
      c.fill();
    });
  }

  function drawLegsArt(c) {
    for (var i = 0; i < legs.length; i++) {
      var leg = legs[i];
      var foot = clampedFoot(leg);
      var knee = solveIK(leg.hip, foot, CONFIG.femur, CONFIG.tibia, leg.side * CONFIG.kneeBend);

      c.strokeStyle = ART.leg;
      c.lineWidth = 5;
      c.beginPath();
      c.moveTo(leg.hip.x, leg.hip.y);
      c.lineTo(knee.x, knee.y);
      c.stroke();

      c.strokeStyle = ART.legDark;
      c.lineWidth = 3.5;
      c.beginPath();
      c.moveTo(knee.x, knee.y);
      c.lineTo(foot.x, foot.y);
      c.stroke();

      c.fillStyle = ART.legDark;
      circle(c, foot, CONFIG.footDotRad + 1);
      c.fill();
    }
  }

  // Three overlapping circles filled as one path, which is what gives the
  // body one continuous blobby silhouette instead of three visible discs.
  // Shadow and highlight are the same union offset and resized, not a
  // gradient — a gradient survives the pixelation pass as a blur, a flat
  // offset shape survives it as a hard-edged band like the reference art.
  function drawBodyArt(c) {
    var chunks = body.chunks;
    var shadow = 3;

    c.fillStyle = ART.bodyShadow;
    c.beginPath();
    for (var i = 0; i < chunks.length; i++) {
      var s = chunks[i];
      c.moveTo(s.pos.x + shadow + s.rad, s.pos.y + shadow);
      c.arc(s.pos.x + shadow, s.pos.y + shadow, s.rad, 0, Math.PI * 2);
    }
    c.fill();

    c.fillStyle = ART.body;
    c.beginPath();
    for (var j = 0; j < chunks.length; j++) {
      var b = chunks[j];
      c.moveTo(b.pos.x + b.rad, b.pos.y);
      c.arc(b.pos.x, b.pos.y, b.rad, 0, Math.PI * 2);
    }
    c.fill();

    c.fillStyle = ART.bodyHighlight;
    c.globalAlpha = 0.55;
    circle(c, v(body.torso.pos.x - shadow, body.torso.pos.y - shadow * 1.5), body.torso.rad * 0.5);
    c.fill();
    c.globalAlpha = 1;
  }

  function drawEyesArt(c) {
    var fwd = v(Math.cos(bodyAngle), Math.sin(bodyAngle));
    var right = perp(fwd);
    var head = body.head;
    [-1, 1].forEach(function (side) {
      var p = v(
        head.pos.x + fwd.x * head.rad * 0.35 + right.x * side * head.rad * 0.55,
        head.pos.y + fwd.y * head.rad * 0.35 + right.y * side * head.rad * 0.55
      );
      c.fillStyle = ART.eye;
      circle(c, p, 1.6);
      c.fill();
    });
  }

  // Bounding box (world px) covering everything the art pass draws, with
  // enough margin for the antennae's full reach. Recomputed every frame since
  // the skeleton is constantly moving — cheap, it's a handful of points.
  function artBounds() {
    var minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    function grow(p, r) {
      if (p.x - r < minX) minX = p.x - r;
      if (p.x + r > maxX) maxX = p.x + r;
      if (p.y - r < minY) minY = p.y - r;
      if (p.y + r > maxY) maxY = p.y + r;
    }
    for (var i = 0; i < body.chunks.length; i++) grow(body.chunks[i].pos, body.chunks[i].rad);
    for (var j = 0; j < legs.length; j++) {
      grow(legs[j].hip, 3);
      grow(clampedFoot(legs[j]), 4);
    }
    var margin = CONFIG.antennaLength + CONFIG.antennaSpread + 6;
    return { minX: minX - margin, minY: minY - margin, maxX: maxX + margin, maxY: maxY + margin };
  }

  // Two buffers: `hiBuf` holds a normal, fully anti-aliased render of the
  // creature at 1 world-px per buffer-px; `loBuf` is the actual pixel-art
  // grid, one cell per `pixelUnit` world px. Going straight from vector
  // shapes to a small canvas still anti-aliases every curved or diagonal
  // edge *at that low resolution*, so neighboring cells end up as partial
  // color blends instead of solid blocks — it looks pixelated but reads as
  // soft. Point-sampling one pixel of `hiBuf` per `loBuf` cell (instead of
  // letting the browser box-filter the shrink) throws that blending away:
  // every cell becomes a single flat color, which is what a nearest-neighbor
  // upscale needs in order to look like hard pixel art rather than a blurred
  // photo blown up.
  var hiBuf = document.createElement('canvas');
  var hiCtx = hiBuf.getContext('2d', { willReadFrequently: true });
  var loBuf = document.createElement('canvas');
  var loCtx = loBuf.getContext('2d');

  function drawArt(ctx, now) {
    var b = artBounds();
    var unit = CONFIG.pixelUnit;
    // Snap the buffer's world origin to the pixel-art grid. The skeleton
    // position is continuous, so without this the grid lands at a different
    // sub-pixel phase every frame, and the whole creature shimmers instead
    // of holding still on its own pixels.
    var gx = Math.floor(b.minX / unit) * unit;
    var gy = Math.floor(b.minY / unit) * unit;
    var bw = Math.max(1, Math.ceil((b.maxX - gx) / unit));
    var bh = Math.max(1, Math.ceil((b.maxY - gy) / unit));
    var hw = bw * unit, hh = bh * unit;

    if (hiBuf.width !== hw) hiBuf.width = hw;
    if (hiBuf.height !== hh) hiBuf.height = hh;

    hiCtx.clearRect(0, 0, hw, hh);
    hiCtx.save();
    hiCtx.translate(-gx, -gy);
    hiCtx.lineJoin = 'round';
    hiCtx.lineCap = 'round';

    drawAntennaeArt(hiCtx, now);
    drawLegsArt(hiCtx);
    drawBodyArt(hiCtx);
    drawEyesArt(hiCtx);

    hiCtx.restore();

    if (loBuf.width !== bw) loBuf.width = bw;
    if (loBuf.height !== bh) loBuf.height = bh;

    var src = hiCtx.getImageData(0, 0, hw, hh).data;
    var out = loCtx.createImageData(bw, bh);
    var half = unit >> 1;
    for (var y = 0; y < bh; y++) {
      for (var x = 0; x < bw; x++) {
        var si = ((y * unit + half) * hw + (x * unit + half)) * 4;
        var di = (y * bw + x) * 4;
        out.data[di] = src[si];
        out.data[di + 1] = src[si + 1];
        out.data[di + 2] = src[si + 2];
        out.data[di + 3] = src[si + 3];
      }
    }
    loCtx.putImageData(out, 0, 0);

    // Nearest-neighbor blit at an integer scale, origin snapped to the grid
    // above: this is what turns the now-solid-color cells into hard pixel
    // blocks on screen instead of a scaled-up blur.
    ctx.imageSmoothingEnabled = false;
    ctx.drawImage(loBuf, 0, 0, bw, bh, gx, gy, hw, hh);
  }

  function drawDebug(ctx, fps) {
    ctx.lineJoin = 'round';
    ctx.lineCap = 'round';

    // --- legs, behind the body ---
    for (var i = 0; i < legs.length; i++) {
      var leg = legs[i];
      var foot = clampedFoot(leg);
      var knee = solveIK(leg.hip, foot, CONFIG.femur, CONFIG.tibia, leg.side * CONFIG.kneeBend);

      ctx.strokeStyle = COLORS.moss;
      ctx.lineWidth = 5;
      ctx.beginPath();
      ctx.moveTo(leg.hip.x, leg.hip.y);
      ctx.lineTo(knee.x, knee.y);
      ctx.stroke();

      ctx.lineWidth = 3.5;
      ctx.beginPath();
      ctx.moveTo(knee.x, knee.y);
      ctx.lineTo(foot.x, foot.y);
      ctx.stroke();

      ctx.fillStyle = COLORS.moss;
      circle(ctx, knee, 2);
      ctx.fill();

      // A stepping foot is drawn bigger and lighter to fake the lift, since
      // top-down gives us no height to show.
      ctx.fillStyle = leg.stepping ? COLORS.blossom : COLORS.ink;
      circle(ctx, foot, leg.stepping ? CONFIG.footDotRad * 2 : CONFIG.footDotRad);
      ctx.fill();

      // Step diagnostics: the stretch that will eventually trigger a step.
      ctx.strokeStyle = COLORS.lavender;
      ctx.globalAlpha = 0.5;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(foot.x, foot.y);
      ctx.lineTo(leg.rest.x, leg.rest.y);
      ctx.stroke();
      circle(ctx, leg.rest, 3);
      ctx.stroke();
      ctx.globalAlpha = 1;
    }

    // --- spine ---
    ctx.strokeStyle = COLORS.ink;
    ctx.globalAlpha = 0.35;
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.moveTo(body.head.pos.x, body.head.pos.y);
    ctx.lineTo(body.torso.pos.x, body.torso.pos.y);
    ctx.lineTo(body.abdomen.pos.x, body.abdomen.pos.y);
    ctx.stroke();
    ctx.globalAlpha = 1;

    // --- body chunks ---
    ctx.lineWidth = 2;
    for (var c = 0; c < body.chunks.length; c++) {
      var chunk = body.chunks[c];
      ctx.strokeStyle = COLORS.ink;
      circle(ctx, chunk.pos, chunk.rad);
      ctx.stroke();
    }

    // --- forward tick off the head ---
    var fwd = v(Math.cos(bodyAngle), Math.sin(bodyAngle));
    ctx.strokeStyle = COLORS.blossom;
    ctx.lineWidth = 2;
    ctx.beginPath();
    ctx.moveTo(body.head.pos.x, body.head.pos.y);
    ctx.lineTo(body.head.pos.x + fwd.x * 18, body.head.pos.y + fwd.y * 18);
    ctx.stroke();

    // --- cursor + standoff ring ---
    ctx.strokeStyle = COLORS.lavender;
    ctx.globalAlpha = 0.4;
    ctx.lineWidth = 1;
    ctx.setLineDash([4, 6]);
    circle(ctx, cursor, CONFIG.standoff);
    ctx.stroke();
    ctx.setLineDash([]);
    ctx.globalAlpha = 1;
    ctx.fillStyle = COLORS.lavender;
    circle(ctx, cursor, 3);
    ctx.fill();

    // --- HUD ---
    ctx.fillStyle = COLORS.ink;
    ctx.globalAlpha = 0.7;
    ctx.font = '11px ui-monospace, "JetBrains Mono", monospace';
    ctx.textBaseline = 'top';
    var lines = [
      'speed  ' + len(body.head.vel).toFixed(2) + ' px/tick',
      'range  ' + dist(body.head.pos, cursor).toFixed(0) + ' / ' + CONFIG.standoff,
      'steps  ' + steppingCount() + ' / ' + legs.length,
      'fps    ' + fps.toFixed(0),
      "'d' toggles debug"
    ];
    for (var l = 0; l < lines.length; l++) {
      ctx.fillText(lines[l], 12, 12 + l * 14);
    }
    ctx.globalAlpha = 1;
  }

  // ----------------------------------------------------------------- init

  function init() {
    readThemeColors();

    var canvas = document.createElement('canvas');
    canvas.id = 'aphid-canvas';
    canvas.setAttribute('aria-hidden', 'true');
    // Absolute, not fixed: the canvas lives in the page, not the viewport, so
    // it scrolls out with everything else instead of hovering in place. Every
    // position in the simulation is a page coordinate as a result — the
    // aphid's own position, the feet, and (see the pointermove/scroll
    // handlers below) the cursor it's chasing.
    canvas.style.cssText =
      'position:absolute;top:0;left:0;pointer-events:none;z-index:9999;';
    document.body.appendChild(canvas);
    var ctx = canvas.getContext('2d');

    function resize() {
      var dpr = window.devicePixelRatio || 1;
      // Full document size, not the viewport: a fixed-size canvas would clip
      // or letterbox as soon as the page is taller than one screen. Take the
      // max of body/documentElement scrollHeight — no single property is
      // reliable for full-page height across browsers.
      var body_ = document.body, docEl = document.documentElement;
      viewW = Math.max(body_.scrollWidth, docEl.scrollWidth, docEl.clientWidth) || window.innerWidth;
      viewH = Math.max(body_.scrollHeight, docEl.scrollHeight, docEl.clientHeight) || window.innerHeight;
      canvas.style.width = viewW + 'px';
      canvas.style.height = viewH + 'px';
      canvas.width = Math.round(viewW * dpr);
      canvas.height = Math.round(viewH * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    }
    resize();

    // Spawn near the middle of whatever's on screen right now, in page
    // coordinates — not the document center, which on a long page could be
    // far below the fold.
    var startX = window.scrollX + window.innerWidth * 0.5;
    var startY = window.scrollY + window.innerHeight * 0.5;
    set(rawCursor, startX, startY);
    cursor = copy(rawCursor);
    body = createBody(startX, startY + CONFIG.standoff);
    legs = createLegs();
    plantAllFeet();

    // clientX/Y are viewport-relative and don't change when the page scrolls
    // under a stationary mouse, so they're cached here and re-applied (with
    // the fresh scroll offset) on scroll too — otherwise scrolling wouldn't
    // move the aphid's target at all, since no pointermove fires on its own.
    var lastClientX = null, lastClientY = null;

    function updateCursorFromClient() {
      if (lastClientX === null) return;
      set(rawCursor, lastClientX + window.scrollX, lastClientY + window.scrollY);
      if (!haveCursor) {
        haveCursor = true;
        cursor = copy(rawCursor);
      }
    }

    window.addEventListener('resize', resize);
    // Document height can grow after DOMContentLoaded — images below the
    // fold finishing their layout being the common case — which would
    // otherwise leave the canvas too short for the aphid to reach that part
    // of the page until the next window resize.
    window.addEventListener('load', resize);
    window.addEventListener('pointermove', function (e) {
      lastClientX = e.clientX;
      lastClientY = e.clientY;
      updateCursorFromClient();
    }, { passive: true });
    window.addEventListener('scroll', updateCursorFromClient, { passive: true });

    window.addEventListener('keydown', function (e) {
      if (e.key === 'd' && !e.metaKey && !e.ctrlKey && !e.altKey) {
        var t = e.target;
        var tag = t && t.tagName;
        if (tag === 'INPUT' || tag === 'TEXTAREA' || (t && t.isContentEditable)) return;
        CONFIG.debug = !CONFIG.debug;
      }
    });

    var stepMs = 1000 / CONFIG.tickRate;
    var stepSec = 1 / CONFIG.tickRate;
    var accumulator = 0;
    var last = performance.now();
    var fps = 60;
    var running = true;

    document.addEventListener('visibilitychange', function () {
      if (document.hidden) {
        running = false;
      } else if (!running) {
        running = true;
        last = performance.now();
        accumulator = 0;
        requestAnimationFrame(frame);
      }
    });

    function frame(now) {
      if (!running) return;
      var elapsed = now - last;
      last = now;
      // Clamp so a long pause doesn't integrate a huge catch-up step.
      if (elapsed > stepMs * CONFIG.maxSubSteps) elapsed = stepMs * CONFIG.maxSubSteps;
      fps = lerp(fps, 1000 / Math.max(elapsed, 1), 0.05);

      accumulator += elapsed;
      var steps = 0;
      while (accumulator >= stepMs && steps < CONFIG.maxSubSteps) {
        stepPhysics(stepSec);
        updateLegs(stepSec);
        accumulator -= stepMs;
        steps++;
      }

      ctx.clearRect(0, 0, viewW, viewH);
      drawArt(ctx, now);
      if (CONFIG.debug) drawDebug(ctx, fps);
      requestAnimationFrame(frame);
    }

    requestAnimationFrame(frame);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
