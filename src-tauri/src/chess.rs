use std::{
    path::PathBuf,
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};

use shakmaty::{fen::Fen, san::San, uci::Uci, CastlingMode, Chess, Color, Position};
use tauri::{
    api::path::{resolve_path, BaseDirectory},
    Manager,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, serde::Serialize, Copy, Clone)]
pub enum Score {
    #[serde(rename = "cp")]
    Cp(i64),
    #[serde(rename = "mate")]
    Mate(i64),
}

#[derive(Clone, serde::Serialize, Debug)]
pub struct BestMovePayload {
    engine: String,
    depth: usize,
    score: Score,
    #[serde(rename = "sanMoves")]
    san_moves: Vec<String>,
    #[serde(rename = "uciMoves")]
    uci_moves: Vec<String>,
    multipv: usize,
    nps: usize,
}

pub fn parse_uci(info: &str, fen: &str, engine: &str) -> Option<BestMovePayload> {
    let mut depth = 0;
    let mut score = Score::Cp(0);
    let mut pv = String::new();
    let mut multipv = 0;
    let mut nps = 0;
    // example input: info depth 1 seldepth 1 multipv 1 score cp 0 nodes 20 nps 10000 tbhits 0 time 2 pv e2e4
    for (i, s) in info.split_whitespace().enumerate() {
        match s {
            "depth" => depth = info.split_whitespace().nth(i + 1).unwrap().parse().unwrap(),
            "score" => {
                if info.split_whitespace().nth(i + 1).unwrap() == "cp" {
                    score = Score::Cp(info.split_whitespace().nth(i + 2).unwrap().parse().unwrap());
                } else {
                    score =
                        Score::Mate(info.split_whitespace().nth(i + 2).unwrap().parse().unwrap());
                }
            }
            "nps" => nps = info.split_whitespace().nth(i + 1).unwrap().parse().unwrap(),
            "multipv" => {
                multipv = info.split_whitespace().nth(i + 1).unwrap().parse().unwrap();
            }
            "pv" => {
                pv = info
                    .split_whitespace()
                    .skip(i + 1)
                    .take_while(|x| !x.starts_with("currmove"))
                    .collect::<Vec<&str>>()
                    .join(" ");
            }
            _ => (),
        }
    }
    let mut san_moves = Vec::new();
    let uci_moves: Vec<String> = pv.split_whitespace().map(|x| x.to_string()).collect();

    let fen: Fen = fen.parse().unwrap();
    let mut pos: Chess = fen.into_position(CastlingMode::Standard).unwrap();
    if pos.turn() == Color::Black {
        score = match score {
            Score::Cp(x) => Score::Cp(-x),
            Score::Mate(x) => Score::Mate(-x),
        };
    }
    for m in &uci_moves {
        let uci: Uci = m.parse().unwrap();
        let m = uci.to_move(&pos).unwrap();
        pos.play_unchecked(&m);
        let san = San::from_move(&pos, &m);
        san_moves.push(san.to_string());
    }
    Some(BestMovePayload {
        depth,
        score,
        san_moves,
        uci_moves,
        multipv,
        engine: engine.to_string(),
        nps,
    })
}

#[tauri::command]
pub async fn get_best_moves(
    engine: String,
    relative: bool,
    fen: String,
    depth: usize,
    number_lines: usize,
    number_threads: usize,
    app: tauri::AppHandle,
) {
    let mut path = PathBuf::from(&engine);
    if relative {
        path = resolve_path(
            &app.config(),
            app.package_info(),
            &app.env(),
            path,
            Some(BaseDirectory::AppData),
        )
        .unwrap();
    }
    // start engine command
    println!("RUNNING ENGINE");
    println!("{}", &path.display());
    println!("{}", &fen);

    // Check number of lines is between 1 and 5
    assert!(number_lines > 0 && number_lines < 6);

    let mut command = Command::new(&path);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);

    let mut child = command
        // .kill_on_drop(true)
        .spawn()
        .expect("Failed to start engine");

    let stdin = child
        .stdin
        .take()
        .expect("child did not have a handle to stdin");
    let stdout = child
        .stdout
        .take()
        .expect("child did not have a handle to stdout");
    let mut stdout_reader = BufReader::new(stdout).lines();

    let (tx, mut rx) = tokio::sync::broadcast::channel(16);

    let id = app.listen_global("stop_engine", move |_| {
        let tx = tx.clone();
        tokio::spawn(async move {
            tx.send(()).unwrap();
        });
    });

    tokio::spawn(async move {
        // run engine process and wait for exit code
        let status = child
            .wait()
            .await
            .expect("engine process encountered an error");
        println!("engine process exit status : {}", status);
    });

    let mut engine_lines = Vec::new();

    // tokio::spawn(async move {
    //     println!("Starting engine");
    //     let mut stdin = stdin;
    //     let write_result = stdin.write_all(b"go\n").await;
    //     if let Err(e) = write_result {
    //         println!("Error writing to stdin: {}", e);
    //     }
    // });

    tokio::spawn(async move {
        let mut stdin = stdin;
        stdin
            .write_all(format!("position fen {}\n", &fen).as_bytes())
            .await
            .expect("Failed to write position");
        stdin
            .write_all(format!("setoption name Threads value {}\n", &number_threads).as_bytes())
            .await
            .expect("Failed to write setoption");
        stdin
            .write_all(format!("setoption name multipv value {}\n", &number_lines).as_bytes())
            .await
            .expect("Failed to write setoption");
        stdin
            .write_all(format!("go depth {}\n", &depth).as_bytes())
            .await
            .expect("Failed to write go");

        let mut last_sent_ms = 0;
        let mut now_ms;
        loop {
            tokio::select! {
                _ = rx.recv() => {
                    println!("Killing engine");
                    stdin.write_all(b"stop\n").await.unwrap();
                    app.unlisten(id);
                    break
                }
                result = stdout_reader.next_line() => {
                    match result {
                        Ok(line_opt) => {
                            if let Some(line) = line_opt {
                                if line == "readyok" {
                                    println!("Engine ready");
                                }
                                if line.starts_with("info") && line.contains("pv") {
                                    let best_moves = parse_uci(&line, &fen, &engine).unwrap();
                                    let multipv = best_moves.multipv;
                                    let depth = best_moves.depth;
                                    engine_lines.push(best_moves);
                                    if multipv == number_lines {
                                        if depth >= 10 && engine_lines.iter().all(|x| x.depth == depth) {
                                            let now = SystemTime::now();
                                            now_ms = now.duration_since(UNIX_EPOCH).unwrap().as_millis();

                                            if now_ms - last_sent_ms > 300 {
                                                app.emit_all("best_moves", &engine_lines).unwrap();
                                                last_sent_ms = now_ms;
                                            }
                                        }
                                        engine_lines.clear();
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            println!("engine read error {:?}", err);
                            break;
                        }
                    }
                }
            }
        }
    });
}