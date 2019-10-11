extern crate decscloud_codec as codec;
extern crate waxosuit_guest as guest;

use codec::gateway::*;
use guest::prelude::*;
use serde::{Deserialize, Serialize};
use stacktrader_types as trader;
use std::collections::HashMap;
use std::sync::RwLock;
use trader::components::*;

lazy_static! {
    static ref POSITIONS: RwLock<HashMap<String, Position>> = RwLock::new(HashMap::new());
}

const RADAR_CONTACTS: &str = "radar_contacts";

pub(crate) fn handle_frame(ctx: &CapabilitiesContext, msg: messaging::BrokerMessage) -> CallResult {
    let frame: codec::systemmgr::EntityFrame = serde_json::from_slice(&msg.body)?;

    let radar_receiver_value = ctx.kv().get(&format!(
        "decs:components:{}:{}:{}",
        frame.shard,
        frame.entity_id,
        super::RADAR_RECEIVER
    ))?;

    let position_value = ctx.kv().get(&format!(
        "decs:components:{}:{}:{}",
        frame.shard,
        frame.entity_id,
        super::POSITION
    ))?;

    let resource_id = format!("decs.components.{}.{}", frame.shard, frame.entity_id);

    if let (Some(radar_str), Some(position_str)) = (radar_receiver_value, position_value) {
        let radar_receiver: RadarReceiver = serde_json::from_str(&radar_str)?;
        let position: Position = serde_json::from_str(&position_str)?;

        let radar_contacts_key = &format!(
            "decs:components:{}:{}:{}",
            frame.shard, frame.entity_id, RADAR_CONTACTS
        );

        let old_contacts: HashMap<String, RadarContact> = ctx
            .kv()
            .set_members(radar_contacts_key)?
            .iter()
            .filter_map(|c| {
                if let Ok(Some(contact_str)) = ctx.kv().get(&c.replace(".", ":")) {
                    if let Some(contact) = serde_json::from_str(&contact_str).unwrap_or(None) {
                        Some((c, contact))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .fold(
                HashMap::<String, RadarContact>::new(),
                |mut acc, (c, contact)| {
                    acc.insert(c.to_string(), contact);
                    acc
                },
            );

        let all_positions = POSITIONS.read().unwrap();
        let updates = radar_updates(
            &frame.entity_id,
            &position,
            &radar_receiver,
            &old_contacts,
            &all_positions,
        );

        let _results = updates
            .iter()
            .map(|update| match update {
                RadarContactDelta::Add(rc) => (
                    ResProtocolRequest::New(format!(
                        "{}.{}",
                        resource_id.to_string(),
                        RADAR_CONTACTS
                    ))
                    .to_string()
                    .clone(),
                    serde_json::json!({"params": rc.clone()}),
                ),
                RadarContactDelta::Remove(rid) => (
                    ResProtocolRequest::Delete(format!(
                        "{}.{}",
                        resource_id.to_string(),
                        RADAR_CONTACTS
                    ))
                    .to_string(),
                    serde_json::json!({"params": {"rid": rid.replace(":", ".")}}),
                ),
                RadarContactDelta::Change(rid, rc) => (
                    format!("call.{}.set", rid.clone()),
                    serde_json::json!({"params": rc.clone()}),
                ),
            })
            .map(|(subject, payload)| publish_message(ctx, &subject, payload))
            .collect::<Vec<CallResult>>();
    }
    Ok(vec![])
}

/// Helper function used to publish a payload on a specified subjct
fn publish_message(
    ctx: &CapabilitiesContext,
    subject: &str,
    payload: serde_json::value::Value,
) -> CallResult {
    if ctx
        .msg()
        .publish(subject, None, &serde_json::to_vec(&payload)?)
        .is_err()
    {
        Err("Error publishing message".into())
    } else {
        Ok(vec![])
    }
}

/// Function to compute all changes to a contact list needed given a resources id, current position,
/// radar receiver, all old contacts, and a map of all entity positions that are published.
/// Changes are in the form of RadarContactDeltas, either specifying to Add, Remove, or Change a contact.
fn radar_updates(
    entity_id: &str,
    current_position: &Position,
    radar_receiver: &RadarReceiver,
    old_contacts: &HashMap<String, RadarContact>,
    all_positions: &HashMap<String, Position>,
) -> Vec<RadarContactDelta> {
    let contacts: Vec<String> = old_contacts
        .values()
        .map(|rc| rc.clone().entity_id)
        .collect();
    all_positions
        .iter()
        .filter_map(|(k, v)| {
            if contacts.contains(k) {
                let mut rid: String = "".to_string();
                if let Some((key, _val)) = old_contacts.iter().find(|(_k, v)| v.entity_id == *k) {
                    rid = key.to_string().replace(":", ".");
                }
                if within_radius(current_position, v, radar_receiver.radius) {
                    let vector_to = current_position.vector_to(v);
                    Some(RadarContactDelta::Change(
                        rid,
                        RadarContact {
                            entity_id: k.clone().to_string(),
                            distance: vector_to.mag,
                            azimuth: vector_to.azimuth,
                            elevation: vector_to.elevation,
                        },
                    ))
                } else {
                    Some(RadarContactDelta::Remove(rid))
                }
            } else if entity_id != k && within_radius(current_position, &v, radar_receiver.radius) {
                let vector_to = current_position.vector_to(v);
                Some(RadarContactDelta::Add(RadarContact {
                    entity_id: k.clone().to_string(),
                    distance: vector_to.mag,
                    azimuth: vector_to.azimuth,
                    elevation: vector_to.elevation,
                }))
            } else {
                None
            }
        })
        .collect::<Vec<RadarContactDelta>>()
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum RadarContactDelta {
    Add(RadarContact),
    Remove(String),
    Change(String, RadarContact),
}

/// Receives messages on the subject `event.decs.components.{shard}.{entity}.position.change`
/// Stores entity position in-memory in the POSITIONS HashMap
/// The cache is used later to discover nearby radar_contacts
pub(crate) fn handle_entity_position_change(
    _ctx: &CapabilitiesContext,
    msg: messaging::BrokerMessage,
) -> CallResult {
    let subject: Vec<&str> = msg.subject.split('.').collect();
    let position_value: serde_json::Value = serde_json::from_slice(&msg.body)?;
    let position: Position = serde_json::from_value::<Position>(position_value["values"].clone())?;
    POSITIONS
        .write()
        .unwrap()
        .insert(subject[4].to_string(), position);
    Ok(vec![])
}

/// Helper function to clean up determining if an entity is within a radius
fn within_radius(entity: &Position, target: &Position, radius: f64) -> bool {
    entity.distance_to(target) <= radius
}

#[cfg(test)]
mod test {
    use super::radar_updates;
    use super::within_radius;
    use super::HashMap;
    use super::Position;
    use super::RadarContact;
    use super::RadarContactDelta;
    use super::RadarReceiver;

    #[test]
    fn test_within_radius() {
        let a = Position {
            x: 1.0,
            y: 1.0,
            z: 1.0,
        };
        let b = Position {
            x: 1.0,
            y: 2.0,
            z: 1.0,
        };
        let radius = 3.0;
        assert!(within_radius(&a, &b, radius));
    }

    #[test]
    fn test_outside_radius() {
        let a = Position {
            x: 1.0,
            y: 1.0,
            z: 1.0,
        };
        let b = Position {
            x: 1.0,
            y: 200.0,
            z: 1.0,
        };
        let radius = 3.0;
        assert!(!within_radius(&a, &b, radius));
    }

    #[test]
    fn test_exact_radius() {
        let a = Position {
            x: 0.0,
            y: 20.0,
            z: 0.0,
        };
        let b = Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let radius = 20.0;
        assert!(within_radius(&a, &b, radius));
    }

    #[test]
    fn test_add_contacts() {
        let rid = "decs.components.the_shard.myownentity".to_string();
        let current_position = Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let radar_receiver = RadarReceiver { radius: 5.0 };
        let old_contacts: HashMap<String, RadarContact> = HashMap::new();
        let mut all_positions: HashMap<String, Position> = HashMap::new();

        let vector_to = current_position.vector_to(&current_position.clone());

        let nearby_asteroid = RadarContact {
            entity_id: "decs.components.the_shard.asteroid".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };
        let nearby_ship = RadarContact {
            entity_id: "decs.components.the_shard.ship".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };
        let mut far_away_money = RadarContact {
            entity_id: "decs.components.the_shard.money".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };
        let mut far_away_money_pos = current_position.clone();
        far_away_money_pos.x += 500.0;
        let new_vector_to = current_position.vector_to(&far_away_money_pos);
        far_away_money.distance = new_vector_to.mag;
        far_away_money.azimuth = new_vector_to.azimuth;
        far_away_money.elevation = new_vector_to.elevation;

        all_positions.insert(rid.to_string(), current_position);
        all_positions.insert(nearby_asteroid.entity_id.clone(), current_position.clone());
        all_positions.insert(nearby_ship.entity_id.clone(), current_position.clone());
        all_positions.insert(far_away_money.entity_id.clone(), far_away_money_pos.clone());

        let changes = radar_updates(
            &rid,
            &current_position,
            &radar_receiver,
            &old_contacts,
            &all_positions,
        );

        assert_eq!(changes.len(), 2);
        // The following loop ensures that all of the changes don't include the far_away_money, but they do include nearby_asteroid & nearby_ship
        let mut found_rc_entity_id = far_away_money.clone().entity_id;
        for c in changes {
            match c {
                RadarContactDelta::Add(rc) => {
                    assert!(
                        rc.entity_id != found_rc_entity_id
                            && (rc.entity_id == nearby_asteroid.entity_id
                                || rc.entity_id == nearby_ship.entity_id)
                    );
                    found_rc_entity_id = rc.clone().entity_id;
                }
                RadarContactDelta::Remove(_) => assert!(false),
                RadarContactDelta::Change(_, _) => assert!(false),
            }
        }
    }

    #[test]
    fn test_remove_contacts() {
        let rid = "decs.components.the_shard.myownentity".to_string();
        let current_position = Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let radar_receiver = RadarReceiver { radius: 5.0 };
        let mut all_positions: HashMap<String, Position> = HashMap::new();

        let vector_to = current_position.vector_to(&current_position.clone());

        let mut nearby_asteroid = RadarContact {
            entity_id: "decs.components.the_shard.asteroid".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };
        let mut nearby_ship = RadarContact {
            entity_id: "decs.components.the_shard.ship".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };
        let mut far_away_money = RadarContact {
            entity_id: "decs.components.the_shard.money".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };

        let mut old_contacts: HashMap<String, RadarContact> = HashMap::new();
        let remove_rid_1 = "decs.components.the_shard.myownentity.1".to_string();
        let remove_rid_2 = "decs.components.the_shard.myownentity.2".to_string();
        old_contacts.insert(remove_rid_1.clone(), nearby_asteroid.clone());
        old_contacts.insert(remove_rid_2.clone(), nearby_ship.clone());

        let mut current_position_clone = current_position.clone();

        current_position_clone.x += 500.0;
        let new_vector_to = current_position.vector_to(&current_position_clone);
        far_away_money.distance = new_vector_to.mag;
        far_away_money.azimuth = new_vector_to.azimuth;
        far_away_money.elevation = new_vector_to.elevation;
        nearby_asteroid.distance = new_vector_to.mag;
        nearby_asteroid.azimuth = new_vector_to.azimuth;
        nearby_asteroid.elevation = new_vector_to.elevation;
        nearby_ship.distance = new_vector_to.mag;
        nearby_ship.azimuth = new_vector_to.azimuth;
        nearby_ship.elevation = new_vector_to.elevation;

        all_positions.insert(rid.to_string(), current_position);
        all_positions.insert(
            nearby_asteroid.entity_id.clone(),
            current_position_clone.clone(),
        );
        all_positions.insert(
            nearby_ship.entity_id.clone(),
            current_position_clone.clone(),
        );
        all_positions.insert(
            far_away_money.entity_id.clone(),
            current_position_clone.clone(),
        );

        let changes = radar_updates(
            &rid,
            &current_position,
            &radar_receiver,
            &old_contacts,
            &all_positions,
        );

        assert_eq!(changes.len(), 2);
        assert!(changes.contains(&RadarContactDelta::Remove(remove_rid_1)));
        assert!(changes.contains(&RadarContactDelta::Remove(remove_rid_2)));
        assert!(!changes.contains(&RadarContactDelta::Remove(
            far_away_money.entity_id.to_string()
        )));
        assert!(!changes.contains(&RadarContactDelta::Remove(rid.to_string())));
        for c in changes {
            match c {
                RadarContactDelta::Add(_) => assert!(false),
                RadarContactDelta::Change(_, _) => assert!(false),
                RadarContactDelta::Remove(_) => assert!(true),
            }
        }
    }

    #[test]
    fn test_change_contact() {
        let rid = "decs.components.the_shard.myownentity".to_string();
        let current_position = Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let radar_receiver = RadarReceiver { radius: 5.0 };
        let mut all_positions: HashMap<String, Position> = HashMap::new();

        let vector_to = current_position.vector_to(&current_position.clone());

        let mut nearby_asteroid = RadarContact {
            entity_id: "decs.components.the_shard.asteroid".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };
        let mut nearby_ship = RadarContact {
            entity_id: "decs.components.the_shard.ship".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };
        let mut far_away_money = RadarContact {
            entity_id: "decs.components.the_shard.money".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };

        let mut old_contacts: HashMap<String, RadarContact> = HashMap::new();

        let change_rid_1 = "decs.components.the_shard.myownentity.1".to_string();
        let change_rid_2 = "decs.components.the_shard.myownentity.2".to_string();
        let change_rid_3 = "decs.components.the_shard.myownentity.3".to_string();
        old_contacts.insert(change_rid_1.clone(), nearby_asteroid.clone());
        old_contacts.insert(change_rid_2.clone(), nearby_ship.clone());
        old_contacts.insert(change_rid_3.clone(), far_away_money.clone());

        let mut current_position_clone = current_position.clone();
        current_position_clone.x += 2.0;
        let new_vector_to = current_position.vector_to(&current_position_clone);
        far_away_money.distance = new_vector_to.mag;
        far_away_money.azimuth = new_vector_to.azimuth;
        far_away_money.elevation = new_vector_to.elevation;
        nearby_asteroid.distance = new_vector_to.mag;
        nearby_asteroid.azimuth = new_vector_to.azimuth;
        nearby_asteroid.elevation = new_vector_to.elevation;
        nearby_ship.distance = new_vector_to.mag;
        nearby_ship.azimuth = new_vector_to.azimuth;
        nearby_ship.elevation = new_vector_to.elevation;

        all_positions.insert(rid.to_string(), current_position);
        all_positions.insert(
            nearby_asteroid.entity_id.clone(),
            current_position_clone.clone(),
        );
        all_positions.insert(
            nearby_ship.entity_id.clone(),
            current_position_clone.clone(),
        );
        all_positions.insert(
            far_away_money.entity_id.clone(),
            current_position_clone.clone(),
        );

        let changes = radar_updates(
            &rid,
            &current_position,
            &radar_receiver,
            &old_contacts,
            &all_positions,
        );

        assert_eq!(changes.len(), 3);
        for c in changes {
            match c {
                RadarContactDelta::Add(_rc) => assert!(false),
                RadarContactDelta::Remove(_s) => assert!(false),
                RadarContactDelta::Change(s, _rc) => {
                    assert!(s == change_rid_1 || s == change_rid_2 || s == change_rid_3)
                }
            }
        }
    }

    #[test]
    fn test_modify_all_contacts() {
        let rid = "decs.components.the_shard.myownentity".to_string();
        let current_position = Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let radar_receiver = RadarReceiver { radius: 5.0 };
        let mut all_positions: HashMap<String, Position> = HashMap::new();

        let vector_to = current_position.vector_to(&current_position.clone());

        let mut nearby_asteroid = RadarContact {
            entity_id: "decs.components.the_shard.asteroid".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };
        let mut nearby_ship = RadarContact {
            entity_id: "decs.components.the_shard.ship".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };
        let mut far_away_money = RadarContact {
            entity_id: "decs.components.the_shard.money".to_string(),
            distance: vector_to.mag,
            azimuth: vector_to.azimuth,
            elevation: vector_to.elevation,
        };

        let mut old_contacts: HashMap<String, RadarContact> = HashMap::new();
        let change_rid_1 = "decs.components.the_shard.myownentity.1".to_string();
        let remove_rid_2 = "decs.components.the_shard.myownentity.2".to_string();
        old_contacts.insert(change_rid_1.clone(), nearby_asteroid.clone());
        old_contacts.insert(remove_rid_2.clone(), far_away_money.clone());
        all_positions.insert(rid.to_string(), current_position);

        // Change asteroid to move it slightly away
        let mut current_position_clone = current_position.clone();
        current_position_clone.x += 2.0;
        let mut new_vector_to = current_position.vector_to(&current_position_clone);
        nearby_asteroid.distance = new_vector_to.mag;
        nearby_asteroid.azimuth = new_vector_to.azimuth;
        nearby_asteroid.elevation = new_vector_to.elevation;
        all_positions.insert(
            nearby_asteroid.entity_id.clone(),
            current_position_clone.clone(),
        );

        // Remove money, move it very far away
        current_position_clone.x += 500.0;
        new_vector_to = current_position.vector_to(&current_position_clone);
        far_away_money.distance = new_vector_to.mag;
        far_away_money.azimuth = new_vector_to.azimuth;
        far_away_money.elevation = new_vector_to.elevation;
        all_positions.insert(
            far_away_money.entity_id.clone(),
            current_position_clone.clone(),
        );

        // Add a new nearby ship, which wasn't an old contact.
        new_vector_to = current_position.vector_to(&current_position.clone());
        nearby_ship.distance = new_vector_to.mag;
        nearby_ship.azimuth = new_vector_to.azimuth;
        nearby_ship.elevation = new_vector_to.elevation;
        all_positions.insert(nearby_ship.entity_id.clone(), current_position.clone());

        let changes = radar_updates(
            &rid,
            &current_position,
            &radar_receiver,
            &old_contacts,
            &all_positions,
        );

        assert_eq!(changes.len(), 3);
        for c in changes {
            match c {
                RadarContactDelta::Add(rc) => {
                    assert!(rc.entity_id == "decs.components.the_shard.ship")
                }
                RadarContactDelta::Remove(s) => assert_eq!(s, remove_rid_2),
                RadarContactDelta::Change(s, _rc) => assert_eq!(s, change_rid_1),
            }
        }
    }
}
