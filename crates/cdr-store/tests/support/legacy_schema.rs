//! Build a pre-retirement fixture before removing old extension columns.
//! A real database with retirement evidence must never use this downgrade helper.
pub fn remove_retirement_guards(db: &rusqlite::Connection) {
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM cdr_archived_room_dispositions",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let guards=db.prepare("SELECT type,name FROM sqlite_schema WHERE (type='trigger' AND name LIKE 'cdr_retired_%') OR (type='view' AND name LIKE 'cdr_live_%')").unwrap()
        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
    for (kind, name) in guards {
        db.execute_batch(&format!("DROP {kind} \"{}\"", name.replace('"', "\"\"")))
            .unwrap();
    }
}
