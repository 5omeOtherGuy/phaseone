//! The `skills` port exposes only the agent's assembled prompt-data snapshot.
use p1_contracts::skill::{LoadedSkill, SkillSummary};
use wasmtime::bail;
use wasmtime::component::{Linker, Val};

use crate::capabilities::{CallState, check_arity};
use crate::loader::interface_import;

pub(crate) fn link(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut skills = linker.instance(&interface_import("skills"))?;
    skills.func_new_async("list", |store, _ty, params, results| {
        let signature = check_arity("skills.list", params, results, 0, 1);
        Box::new(async move {
            signature?;
            let Some(source) = &store.data().skills else {
                bail!("skills called without a skill source");
            };
            results[0] = Val::List(source.list().skills.into_iter().map(summary_val).collect());
            Ok(())
        })
    })?;
    skills.func_new_async("load", |store, _ty, params, results| {
        let name = (|| {
            check_arity("skills.load", params, results, 1, 1)?;
            let Val::String(name) = &params[0] else {
                bail!("skills.load: invalid parameter type");
            };
            Ok(name.clone())
        })();
        Box::new(async move {
            let name = name?;
            let Some(source) = &store.data().skills else {
                bail!("skills called without a skill source");
            };
            results[0] = Val::Result(match source.load(&name) {
                Ok(skill) => Ok(Some(Box::new(loaded_val(skill)))),
                Err(error) => Err(Some(Box::new(Val::Variant(
                    "message".into(),
                    Some(Box::new(Val::String(error.to_string()))),
                )))),
            });
            Ok(())
        })
    })
}

fn summary_val(skill: SkillSummary) -> Val {
    Val::Record(vec![
        ("name".into(), Val::String(skill.name)),
        ("description".into(), Val::String(skill.description)),
        ("path".into(), Val::String(skill.path.display().to_string())),
        ("bytes".into(), Val::U64(skill.bytes)),
    ])
}

fn loaded_val(skill: LoadedSkill) -> Val {
    Val::Record(vec![
        ("body".into(), Val::String(skill.body)),
        (
            "directory".into(),
            Val::String(skill.directory.display().to_string()),
        ),
        ("truncated".into(), Val::Bool(skill.truncated)),
    ])
}
