import QtQuick
import QtTest
import "../qml/Presentation.js" as P
import "../qml"

TestCase {
    name: "TranscriptPresentation"
    width: 900
    height: 700
    visible: true
    when: windowShown
    function record(id, role, text, fields) {
        return {record_id:id,record:Object.assign({role:role,text:text},fields || {})};
    }
    function test_context_projection_preserves_source() {
        var text="<recommended_plugins>plugins</recommended_plugins>\n# AGENTS.md instructions for /repo\n<INSTRUCTIONS>rules</INSTRUCTIONS><environment_context>env</environment_context>\nFix the toolbar";
        var source=record("source","user",text,{future:{answer:42}}), projected=P.project([source]);
        compare(projected.length,4);
        compare(projected[3].record_id,"source");
        compare(projected[3].record.text,"\nFix the toolbar");
        compare(projected.map(function(e) { return e.record.text; }).join(""),text);
        projected.forEach(function(e) { compare(e.source_id,"source"); compare(JSON.parse(e.raw).record.future.answer,42); });
        compare(source.record.text,text);
        compare(JSON.stringify(P.project(projected)),JSON.stringify(projected));
    }
    function test_context_false_positives() {
        ["Explain <environment_context>x</environment_context>","```xml\n<environment_context>x</environment_context>\n```","> <recommended_plugins>x</recommended_plugins>","<div>hello</div>","<environment_context>unfinished","# AGENTS.md instructions for /repo\nDiscuss this example"].forEach(function(text) {
            var result=P.project([record("a","user",text)]); compare(result.length,1); verify(!result[0].record.context_label); compare(result[0].record.text,text);
        });
    }
    function test_question_reply() {
        var source=record("q","user",'<send_user_message_question_reply>[{"questionItemId":"1","question":"Where?","answer":"Staging"}]</send_user_message_question_reply>');
        var p=P.project([source]); compare(p.length,2); compare(p[0].record.context_label,"Question"); compare(p[1].record.text,"Staging"); compare(p[1].record_id,"q");
    }
    function test_assistant_transport() {
        var annotation=':codex-annotation{index="1"}', citation="<oai-mem-citation>\n<citation_entries>x</citation_entries>\n<rollout_ids>y</rollout_ids>\n</oai-mem-citation>";
        compare(P.assistantText("Answer "+annotation+" here.\n"+citation),"Answer  here.");
        ["`"+annotation+"`","```text\n"+annotation+"\n```","> "+annotation,"    "+annotation,'"'+annotation+'"',"`multiline\n"+annotation+"\ncode`","~~~xml\n"+citation+"\n~~~",citation+"\nExplanation", "<oai-mem-citation>unfinished"].forEach(function(text) { compare(P.assistantText(text),text); });
    }
    function test_completion_requires_both_and_same_turn() {
        var c=record("c","assistant","Working",{source_turn_id:"t",assistant_phase:"commentary"}), t=record("tool","tool_use","read",{source_turn_id:"t"}), f=record("f","assistant","Done",{source_turn_id:"t",assistant_phase:"final_answer"}), done=record("done","lifecycle","",{source_turn_id:"t",lifecycle_event:"task_complete"});
        var result=P.group([c,t,f,done]); compare(result.length,2); verify(result[0].completed); compare(result[0].records.length,2); verify(!result[1].completed);
        [[c,t,f],[c,t,done],[c,t,f,record("other","lifecycle","",{source_turn_id:"other",lifecycle_event:"task_complete"})],[c,t,f,done,record("abort","lifecycle","",{source_turn_id:"t",lifecycle_event:"turn_aborted"})]].forEach(function(entries) { verify(!P.group(entries).some(function(i) { return i.completed; })); });
    }
    function test_failure_visible_and_strict_types() {
        verify(!P.failure('{"exit_code":false,"is_error":1}'));
        verify(P.failure('{"exit_code":2}'));
        verify(P.failure('{"isError":true}'));
        var call=record("call","tool_use",'{"is_error":true}',{event_id:"call",tool_input:'{"is_error":true}'}), failed=record("result","tool_result","bad",{parent_tool_use_id:"call",tool_result_is_error:true});
        verify(!P.attention([call])); verify(P.attention([failed]));
        var grouped=P.group([record("a","tool_use","first"),call,failed,record("b","tool_use","last")]);
        compare(grouped.length,3); verify(grouped[1].attention); compare(grouped[1].records.length,2);
    }
    function test_linked_pairing_and_concurrent_ambiguity() {
        var a=record("a","tool_use","a",{event_id:"a",tool_name:"read"}), b=record("b","tool_use","b",{event_id:"b",tool_name:"read"}), ar=record("ar","tool_result","a output",{parent_tool_use_id:"a"}), br=record("br","tool_result","b output",{parent_tool_use_id:"b"});
        var paired=P.pair([a,b,br,ar]); compare(paired.length,2); compare(paired[0][1].record_id,"ar"); compare(paired[1][1].record_id,"br");
        compare(P.pair([a,b,record("r","tool_result","unknown")]).length,3);
        compare(P.pair([a,record("r","tool_result","wrong",{parent_tool_use_id:"b"})]).length,2);
        compare(P.pair([a,record("r","tool_result","legacy")]).length,1);
    }
    function test_attachments_only_from_typed_content() {
        var plain=record("a","user","<<ImageDisplayed>>"); compare(P.attachments(plain.record).length,0); compare(P.project([plain])[0].record.text,"<<ImageDisplayed>>");
        var typed=record("a","user","<<ImageDisplayed>>\nQuestion\n<<ImageDisplayed>>",{source_content:JSON.stringify([{type:"image",source:{url:"https://example.org/image.png"}},{type:"input_file",file_id:"file123"}])});
        compare(P.attachments(typed.record).length,2); compare(P.project([typed])[0].record.text,"Question\n<<ImageDisplayed>>");
        compare(P.attachments({source_content:'[{"type":"image","url":"javascript:alert(1)"}]'}).length,0);
    }
    function test_raw_retains_boundaries() {
        var records=[record("start","lifecycle","",{lifecycle_event:"task_started"}),record("a","user","hello")];
        compare(P.group(records).length,1); compare(P.group(records,true).length,2);
    }
    function test_find_prefers_request() {
        var items=P.group([record("s","user","<environment_context>env</environment_context>Request")]);
        compare(P.indexForRecord(items,"s"),1);
    }
    function test_fenced_code_blocks_and_safe_highlights() {
        var blocks=P.markdownBlocks("Before\n```rust\nlet value = 42;\n```\nAfter");
        compare(blocks.length,3); verify(blocks[1].code); compare(blocks[1].language,"rust"); compare(blocks[1].text,"let value = 42;");
        var highlighted=P.highlightCode('let text = "<script>";',"rust");
        verify(highlighted.indexOf("<script>")<0); verify(highlighted.indexOf("&lt;script&gt;")>=0); verify(highlighted.indexOf("<span")>=0);
    }
    function test_tool_fields_and_remote_image_guard() {
        var fields=P.toolSections({role:"tool_use",tool_name:"exec_command",tool_input:'{"cmd":"ls -la","cwd":"/tmp"}',text:""});
        compare(fields.length,2); compare(fields[0].language,"sh"); compare(fields[0].text,"ls -la");
        compare(P.inlineImageUrl("/tmp/image.png",{machine:"nicbook-atm"}),"");
        compare(P.inlineImageUrl("file:///tmp/image.png",{machine:"nicbook"}),"");
        compare(P.inlineImageUrl("/tmp/image.png",{machine:"local"}),"file:///tmp/image.png");
        compare(P.inlineImageUrl("https://example.org/a.png",{machine:"local"}),"");
    }
    Component { id: transcriptComponent; Transcript { width: 850; height: 600 } }
    Component { id: codeComponent; CodeBlock { width: 800 } }
    Component { id: toolComponent; ToolBody { width: 800 } }
    function test_native_code_and_tool_paths() {
        failOnWarning(/Binding loop|ReferenceError|TypeError/);
        var code=createTemporaryObject(codeComponent,this,{text:"const x = 42;\n" + "long ".repeat(200),language:"javascript"});
        verify(code!==null);
        var scroll=findChild(code,"codeScroll");
        verify(scroll!==null);
        tryVerify(function() { return scroll.contentWidth > scroll.width; }, 3000, "Long code must scroll horizontally");
        var tool=createTemporaryObject(toolComponent,this,{record:{role:"tool_use",tool_name:"exec_command",tool_input:'{"cmd":"echo hello","path":"/tmp/source.rs"}',text:""}});
        verify(tool!==null);
        tryVerify(function() { return findChild(tool,"codeText")!==null; });
    }
    function test_native_image_and_remote_guard() {
        failOnWarning(/Binding loop|ReferenceError|TypeError/);
        var data="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
        var view=createTemporaryObject(transcriptComponent,this,{session:{machine:"local"},records:[record("image","user","Image",{source_content:JSON.stringify([{type:"image",image_url:data}])})]});
        verify(view!==null);
        tryVerify(function() { var image=findChild(view,"attachmentThumbnail"); return image!==null && image.visible; });
        var thumbnail=findChild(view,"attachmentThumbnail");
        tryCompare(thumbnail,"status",Image.Ready);
        mouseClick(thumbnail,thumbnail.width/2,thumbnail.height/2);
        tryCompare(findChild(view,"imageDialog"),"visible",true);
        findChild(view,"imageDialog").close();
        view.session={machine:"nicbook-atm"};
        view.records=[record("remote","user","Remote image",{source_content:JSON.stringify([{type:"image",path:"/private/remote-only.png"}])})];
        tryVerify(function() { var image=findChild(view,"attachmentThumbnail"); return image!==null && !image.visible && !String(image.source); });
    }
    function test_native_transcript_creation_and_jump() {
        failOnWarning(/Binding loop|ReferenceError|TypeError/);
        var view=createTemporaryObject(transcriptComponent,this,{records:[record("u","user","# Heading\n\nNative **Markdown**\n```rust\nlet n = 42;\n```"),record("a","assistant","Answer needle"),record("tool","tool_result",'{"exit_code":2,"output":"failed"}')]});
        verify(view!==null, "Transcript must instantiate");
        compare(view.items.length,3);
        tryVerify(function() { return findChild(view,"codeScroll")!==null; }, 3000, "Fenced code must instantiate a native scroll view");
        view.findText="needle";
        verify(view.jumpToRecord("a"), "Existing record must be jumpable");
        compare(view.targetRecord,"a");
        verify(!view.jumpToRecord("missing"), "Unknown record must not claim a successful jump");
        wait(20); verify(findChild(view,"transcriptList")!==null);
    }
    function test_find_source_body_contract() {
        compare(P.body(record("user","user","text",{tool_input:"ignored",source_content:"not searched"})),"text");
        compare(P.body(record("tool","tool_result","output",{tool_input:"input",tool_output:"output",source_content:"not searched"})),"input\n\noutput");
    }
    function test_find_preserves_source_occurrences_data() {
        return [
            {tag:"injected context",text:"# AGENTS.md instructions\n<INSTRUCTIONS>needle rules</INSTRUCTIONS>\nActual request",query:"needle",occurrence:0,role:"user"},
            {tag:"second answer",text:'<send_user_message_question_reply>[{"questionItemId":"1","question":"needle?","answer":"needle"},{"questionItemId":"2","question":"needle?","answer":"needle"}]</send_user_message_question_reply>',query:"needle",occurrence:3,role:"user"},
            {tag:"stripped marker",text:'Answer :codex-annotation{index="1"}',query:"codex-annotation",occurrence:0,role:"assistant"},
            {tag:"code language",text:"```needle\nbody\n```",query:"needle",occurrence:0,role:"assistant"}
        ];
    }
    function test_find_preserves_source_occurrences(data) {
        failOnWarning(/Binding loop|ReferenceError|TypeError/);
        var view=createTemporaryObject(transcriptComponent,this,{records:[record("source",data.role,data.text)],findText:data.query});
        verify(view.jumpToRecord("source",data.occurrence));
        tryVerify(function() { var editor=findChild(view,"transcriptText"); return editor && editor.selectedText===data.query; });
        var editor=findChild(view,"transcriptText"), expected=-1;
        for (var i=0;i<=data.occurrence;i++) expected=data.text.indexOf(data.query,expected+1);
        compare(editor.selectionStart,expected);
        compare(editor.text,data.text);
    }
    function test_reader_state_survives_recreation_and_paging() {
        failOnWarning(/Binding loop|ReferenceError|TypeError/);
        var records=[record("tool","tool_use","content")];
        var view=createTemporaryObject(transcriptComponent,this,{records:records,session:{machine:"local"}});
        view.updateGroupState(view.items[0],"open",true);
        view.updateGroupState(view.items[0],"raw",true);
        view.updateGroupState(view.items[0],"activityLimit",36);
        view.updateState("tool","rawShowAll",true);
        view.updateState("tool","tool:0:showAll",true);
        var saved=JSON.parse(JSON.stringify(view.expandedItems));
        view.destroy();
        var restored=createTemporaryObject(transcriptComponent,this,{records:records,session:{machine:"local"},expandedItems:saved});
        tryVerify(function() { return findChild(restored,"transcriptCard:tool")!==null; });
        var card=findChild(restored,"transcriptCard:tool");
        verify(card.expanded); verify(card.localRaw); compare(card.activityLimit,36);
        compare(findChild(card,"transcriptText").parent.showAll,true);
        compare(restored.stateValue("tool","rawShowAll",false),true);
        compare(restored.stateValue("tool","tool:0:showAll",false),true);
        restored.width=620;
        restored.records=[record("earlier","tool_use","Earlier")].concat(records);
        tryVerify(function() { return findChild(restored,"transcriptCard:earlier")!==null; });
        card=findChild(restored,"transcriptCard:earlier");
        verify(card.expanded); verify(card.localRaw); compare(card.activityLimit,36);
        compare(findChild(card,"transcriptText").parent.showAll,true);
        restored.session={machine:"local",session_id:"restored"};
        compare(restored.stateValue("tool","rawShowAll",false),true);
    }
    function test_markdown_images_are_explicit_and_guarded() {
        failOnWarning(/Binding loop|ReferenceError|TypeError|Cannot open/);
        var text='![remote](https://example.invalid/never-fetch.png)\n![ref][picture]\n[picture]: https://example.invalid/ref.png\n<img src="https://example.invalid/html.png" alt="HTML">';
        var parsed=P.safeMarkdown(text);
        compare(parsed.images.length,3);
        verify(parsed.text.indexOf("![")<0); verify(parsed.text.indexOf("<img")<0);
        compare(P.safeMarkdown("`![literal](url)`").text,"`![literal](url)`");
        compare(P.inlineImageUrl("images/a.png",{machine:"local",cwd:"/project"}),"file:///project/images/a.png");
        compare(P.inlineImageUrl("../a.png",{machine:"local",cwd:"/project/sub"}),"file:///project/a.png");
        compare(P.inlineImageUrl("images/a.png",{machine:"nicbook-atm",cwd:"/project"}),"");
        var view=createTemporaryObject(transcriptComponent,this,{records:[record("markdown","assistant",text)],session:{machine:"local",cwd:"/project"}});
        tryVerify(function() { return findChild(view,"markdownThumbnail")!==null; });
        var thumbnail=findChild(view,"markdownThumbnail");
        verify(!thumbnail.visible); compare(String(thumbnail.source),"");
        verify(findChild(view,"markdownText").text.indexOf("![")<0);
        view.session={machine:"nicbook-atm",cwd:"/project"};
        view.records=[record("local-reference","assistant","![remote local](images/a.png)")];
        tryVerify(function() { var image=findChild(view,"markdownThumbnail"); return image && !image.visible && !String(image.source); });
    }
    function test_near_end_jump_positions_request_without_expanding_context() {
        failOnWarning(/.*/);
        var records=[];
        for (var i=70;i<130;i++) records.push(record("r"+i,"user","Message "+i));
        records[55]=record("r125","user","<environment_context>Environment</environment_context>Explain the implementation.");
        records[56]=record("r126","assistant","Answer with details.");
        var view=createTemporaryObject(transcriptComponent,this,{records:records});
        verify(view.jumpToRecord("r125"));
        var list=findChild(view,"transcriptList");
        tryVerify(function() {
            var target=findChild(view,"transcriptCard:r125");
            return target && Math.abs(target.y-list.contentY)<1;
        });
        compare(view.groupState(view.items[P.indexForRecord(view.items,"r125:context:0")],"open",false),false);
        var context=findChild(view,"transcriptCard:r125:context:0");
        if (context) verify(!context.expanded);
    }
}
