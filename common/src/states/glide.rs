use super::utils::*;
use crate::{
    comp::{
        character_state::OutputEvents, fluid_dynamics::angle_of_attack, inventory::slot::EquipSlot,
        CharacterState, InputKind, Ori, StateUpdate, Vel,
    },
    event::LocalEvent,
    outcome::Outcome,
    states::{
        behavior::{CharacterBehavior, JoinData},
        glide_wield, idle,
    },
    util::{Dir, Plane, Projection},
};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::{
    f32::consts::PI,
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::Duration,
};
use vek::*;

const PITCH_SLOW_TIME: f32 = 0.5;

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Boost {
    /// Slowly increases XY speed
    Forward(f32),
    /// Gives Z impulse
    Upward(f32),
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Data {
    /// The aspect ratio is the ratio of the span squared to actual planform
    /// area
    pub aspect_ratio: f32,
    pub planform_area: f32,
    pub ori: Ori,
    last_vel: Vel,
    pub timer: Duration,
    inputs_disabled: bool,
    pub booster: Option<Boost>,
}

impl Data {
    /// A glider is modelled as an elliptical wing and has a span length
    /// (distance from wing tip to wing tip) and a chord length (distance from
    /// leading edge to trailing edge through its centre) measured in block
    /// units.
    ///
    ///  https://en.wikipedia.org/wiki/Elliptical_wing
    pub fn new(span_length: f32, chord_length: f32, ori: Ori) -> Self {
        let planform_area = PI * chord_length * span_length * 0.25;
        let aspect_ratio = span_length.powi(2) / planform_area;
        Self {
            aspect_ratio,
            planform_area,
            ori,
            last_vel: Vel::zero(),
            timer: Duration::default(),
            inputs_disabled: true,
            booster: None,
        }
    }

    fn tgt_dir(&self, data: &JoinData) -> Dir {
        let move_dir = if self.inputs_disabled {
            Vec2::zero()
        } else {
            data.inputs.move_dir
        };
        let look_ori = Ori::from(data.inputs.look_dir);
        look_ori
            .yawed_right(PI / 3.0 * look_ori.right().xy().dot(move_dir))
            .pitched_up(PI * 0.04)
            .pitched_down(
                data.inputs
                    .look_dir
                    .xy()
                    .try_normalized()
                    .map_or(0.0, |ld| {
                        PI * 0.1 * ld.dot(move_dir) * self.timer.as_secs_f32().min(PITCH_SLOW_TIME)
                            / PITCH_SLOW_TIME
                    }),
            )
            .look_dir()
    }
}

// pub static TOO_FAST: Lazy<Arc<Mutex<bool>>> = Lazy::new(||
// Arc::new(Mutex::new(false)));
pub static TOO_FAST: AtomicBool = AtomicBool::new(false);
pub static BOOST: AtomicBool = AtomicBool::new(false);
pub static CAP: AtomicBool = AtomicBool::new(true);

impl CharacterBehavior for Data {
    fn behavior(&self, data: &JoinData, output_events: &mut OutputEvents) -> StateUpdate {
        let mut update = StateUpdate::from(data);
        // reset booster
        update.character = CharacterState::Glide(Self {
            booster: None,
            ..*self
        });

        let gained_booster = self.booster;

        handle_glider_input_or(data, &mut update, output_events, |_, _| {});

        // If switched state, let it do its thing
        if !matches!(update.character, CharacterState::Glide { .. }) {
            return update;
        }

        // If player is on the ground and effectively doesn't have any gliding
        // power left, end the glide
        if data.physics.on_ground.is_some()
            && (data.vel.0 - data.physics.ground_vel).magnitude_squared() < 2_f32.powi(2)
            && gained_booster.is_none()
        {
            update.character = CharacterState::GlideWield(glide_wield::Data::from(data));
        } else if data.physics.in_liquid().is_some()
            || data
                .inventory
                .and_then(|inv| inv.equipped(EquipSlot::Glider))
                .is_none()
        {
            update.character = CharacterState::Idle(idle::Data::default());
        } else if !handle_climb(data, &mut update) {
            let air_flow = data
                .physics
                .in_fluid
                .map(|fluid| fluid.relative_flow(data.vel))
                .unwrap_or_default();

            let inputs_disabled = self.inputs_disabled && !data.inputs.move_dir.is_approx_zero();

            let ori = {
                let slerp_s = {
                    let angle = self.ori.look_dir().angle_between(*data.inputs.look_dir);
                    let rate = 0.4 * PI / angle;
                    (data.dt.0 * rate).min(1.0)
                };

                Dir::from_unnormalized(air_flow.0)
                    .map(|flow_dir| {
                        let tgt_dir = self.tgt_dir(data);
                        let tgt_dir_ori = Ori::from(tgt_dir);
                        let tgt_dir_up = tgt_dir_ori.up();
                        // The desired up vector of our glider.
                        // We begin by projecting the flow dir on the plane with the normal of
                        // our tgt_dir to get an idea of how it will hit the glider
                        let tgt_up = flow_dir
                            .projected(&Plane::from(tgt_dir))
                            .map(|d| {
                                let d = if d.dot(*tgt_dir_up).is_sign_negative() {
                                    // when the final direction of flow is downward we don't roll
                                    // upside down but instead mirror the target up vector
                                    Quaternion::rotation_3d(PI, *tgt_dir_ori.right()) * d
                                } else {
                                    d
                                };
                                // slerp from untilted up towards the direction by a factor of
                                // lateral wind to prevent overly reactive adjustments
                                let lateral_wind_speed =
                                    air_flow.0.projected(&self.ori.right()).magnitude();
                                tgt_dir_up.slerped_to(d, lateral_wind_speed / 15.0)
                            })
                            .unwrap_or_else(Dir::up);
                        let global_roll = tgt_dir_up.rotation_between(tgt_up);
                        let global_pitch = angle_of_attack(&tgt_dir_ori, &flow_dir)
                            * self.timer.as_secs_f32().min(PITCH_SLOW_TIME)
                            / PITCH_SLOW_TIME;

                        self.ori.slerped_towards(
                            tgt_dir_ori.prerotated(global_roll).pitched_up(global_pitch),
                            slerp_s,
                        )
                    })
                    .unwrap_or_else(|| self.ori.slerped_towards(self.ori.uprighted(), slerp_s))
            };

            update.ori = {
                let slerp_s = {
                    let angle = data.ori.look_dir().angle_between(*data.inputs.look_dir);
                    let rate = 0.2 * data.body.base_ori_rate() * PI / angle;
                    (data.dt.0 * rate).min(1.0)
                };

                let rot_from_drag = {
                    let speed_factor =
                        air_flow.0.magnitude_squared().min(40_f32.powi(2)) / 40_f32.powi(2);

                    Quaternion::rotation_3d(
                        -PI / 2.0 * speed_factor,
                        ori.up()
                            .cross(air_flow.0)
                            .try_normalized()
                            .unwrap_or_else(|| *data.ori.right()),
                    )
                };

                let rot_from_accel = {
                    let accel = data.vel.0 - self.last_vel.0;
                    let accel_factor = accel.magnitude_squared().min(1.0) / 1.0;

                    Quaternion::rotation_3d(
                        PI / 2.0
                            * accel_factor
                            * if data.physics.on_ground.is_some() {
                                -1.0
                            } else {
                                1.0
                            },
                        ori.up()
                            .cross(accel)
                            .try_normalized()
                            .unwrap_or_else(|| *data.ori.right()),
                    )
                };

                update.ori.slerped_towards(
                    ori.to_horizontal()
                        .prerotated(rot_from_drag * rot_from_accel),
                    slerp_s,
                )
            };

            //epic boost haxxorz
            if input_is_pressed(data, InputKind::Roll) {
                CAP.store(false, std::sync::atomic::Ordering::Relaxed);
            } else {
                CAP.store(true, std::sync::atomic::Ordering::Relaxed);
            }

            if BOOST.load(std::sync::atomic::Ordering::Relaxed) {
                if !TOO_FAST.load(std::sync::atomic::Ordering::Relaxed) {
                    if data.physics.on_ground.is_some() {
                        // quality of life hack: help with starting
                        //
                        // other velocities are intentionally ignored
                        update.vel.0.z += 500.0 * data.dt.0;
                    } else {
                        update.vel.0.x *= 1.0 + 1.0 * data.dt.0;
                        update.vel.0.y *= 1.0 + 1.0 * data.dt.0;
                    }
                }
            }

            // // If we gained a booster
            // if let Some(booster) = gained_booster {
            //     match booster {}
            // };

            // Don't override gained booster, if any, otherwise set to None
            let next_booster = if let CharacterState::Glide(Data {
                booster: Some(booster),
                ..
            }) = update.character
            {
                Some(booster)
            } else {
                None
            };

            update.character = CharacterState::Glide(Self {
                ori,
                last_vel: *data.vel,
                timer: tick_attack_or_default(data, self.timer, None),
                inputs_disabled,
                booster: next_booster,
                ..*self
            });
        }

        update
    }

    fn unwield(&self, data: &JoinData, output_events: &mut OutputEvents) -> StateUpdate {
        let mut update = StateUpdate::from(data);
        output_events.emit_local(LocalEvent::CreateOutcome(Outcome::Glider {
            pos: data.pos.0,
            wielded: false,
        }));
        update.character = CharacterState::Idle(idle::Data::default());
        update
    }
}
